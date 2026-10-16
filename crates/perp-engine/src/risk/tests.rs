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
    let close =
        execute_liquidation_market_order(&mut ctx, ALICE, &market, Side::Sell, quantity).unwrap();

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
    // (untouched) 50M wallet holds.
    let err = add_position_margin(&mut ctx, 100_000_000).unwrap_err();
    assert!(
        err.to_string().contains("insufficient perp wallet"),
        "{err}"
    );
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
        err.to_string().contains("insufficient position margin"),
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
        err.to_string().contains("resulting equity below initial margin"),
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
            .contains("removePositionMargin: resulting margin below maintenance margin"),
        "G2, the balance term, is the first gate that fails: {err}"
    );
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET);
    assert_eq!(position(&mut ctx, ALICE).margin, MARGIN);

    // $33.33 clears G2 (leaves 166_666_667 ≥ MM) but not G3: equity 166_666_667 < PIM.
    let err = remove_position_margin(&mut ctx, 33_333_333).unwrap_err();
    assert!(
        err.to_string()
            .contains("removePositionMargin: resulting equity below initial margin requirement"),
        "{err}"
    );
    // And the cap is exactly zero: one micro-unit is refused by the same gate.
    let err = remove_position_margin(&mut ctx, 1).unwrap_err();
    assert!(
        err.to_string()
            .contains("removePositionMargin: resulting equity below initial margin requirement"),
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
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    let wallet_before = wallet(&mut ctx, ALICE);

    // Nothing comes out of a fresh position.
    let err = remove_position_margin(&mut ctx, 1).unwrap_err();
    assert!(
        err.to_string()
            .contains("removePositionMargin: resulting equity below initial margin requirement"),
        "{err}"
    );

    add_position_margin(&mut ctx, 10_000_000).unwrap();
    // The full round trip is refused, whole — not clipped to the cap.
    let err = remove_position_margin(&mut ctx, 10_000_000).unwrap_err();
    assert!(
        err.to_string()
            .contains("removePositionMargin: resulting equity below initial margin requirement"),
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
            .contains("removePositionMargin: resulting equity below initial margin requirement"),
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

/// **The clearance fee is levied on the margin this call CONSUMED, so n rounds cost one fee.**
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
/// `assert!` at the end, and it is what fails if the base reverts to `pre_liq_margin`.
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
            // wallet (`apply_position_fill`: a loss beyond the position's own margin routes to the
            // Insurance Fund as bad debt and never debits the user). If a mark move or a
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
                // Funding OFF: it is not a zero-sum transfer inside the identity (each position
                // settles against the index on its own touch, with its own floored payment), and
                // it is not what is under test here.
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
                basis_mode: 0,
                deferred_work: 0,
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
                            flags: 0,
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
                            flags: 0,
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
            .filter(|l| l.data.topics().first() == Some(&crate::interface::IPerpDex::OrderCancelled::SIGNATURE_HASH))
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

    /// **`ADJUSTMENT` for a SOLVENT residual ADL'd at MARK.** The book absorbs half the position and
    /// the rest is a forced trade at the mark price — a real counterparty, but no order on either
    /// side, so `ORDER` would send a consumer looking for an order id and a `Trade` row that do not
    /// exist for this leg.
    ///
    /// The sibling below pins the same two-group shape for an INSOLVENT residual (filled at the
    /// bankruptcy price). The two differ only in price, which is exactly the claim worth pinning
    /// twice: solvent residuals used to take a completely separate, counterparty-free code path.
    #[test]
    fn a_solvent_residual_adld_at_mark_reports_adjustment() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
        // Only HALF the size is bid for, so the other half becomes the residual. The position is
        // below maintenance but not bankrupt at this mark, so the residual is SOLVENT and fills at
        // mark rather than at the bankruptcy price.
        place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64 / 2);
        seed_absorbing_short(&mut ctx, 1);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        storage::begin_perp_call(&mut ctx);
        liquidate(&mut ctx, ALICE).unwrap();
        storage::flush_account_snapshots(&mut ctx).unwrap();

        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_account_update_groups(&logs);
        assert_eq!(
            position(&mut ctx, ALICE).amount,
            0,
            "fixture: the residual really did have to be ADL'd"
        );
        assert_eq!(
            account_update_reasons(&logs),
            vec![
                // the book leg, an ordinary fill for both sides
                (MAKER, R::Order),
                (ALICE, R::Order),
                // the residual: a forced trade at mark — one group per party, no order
                (ALICE, R::Adjustment),
                (WHALE, R::Adjustment),
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
                // the oracle update that drove the sweep, emitted after it — ONE price event,
                // not the `IndexPriceUpdated` + `MarkPriceUpdated` pair it used to be
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
    /// ⚠️ **RE-POINTED.** This used to reach `Multiple` through funding-plus-clearance-fee, on a
    /// fixture where the liquidation closed NOTHING (empty book, insolvent residual, no eligible
    /// ADL counterparty): nothing published for ALICE, so the `FundingFee` mark was still standing
    /// when the clearance fee marked her `InsuranceClear`.
    ///
    /// That shape is gone, because the clearance fee is now levied on the margin the call actually
    /// CONSUMED. A liquidation that closed nothing charges nothing, so there is no second cause —
    /// and the two paths that DO consume margin (the book leg and ADL) both publish for the user
    /// inline, which clears the funding mark. The combination is unreachable by construction.
    ///
    /// So this now pins the new rule where the old one was observable, which is the same corner
    /// from the other side: nothing closed, therefore no fee, therefore exactly ONE cause. The
    /// remaining candidate for a genuine `Multiple` is the rounding write-off in
    /// `execute_liquidation_market_order`'s full-fill path meeting the fee's `InsuranceClear` (that
    /// site's own comment says so); it needs a hand-tuned residue and has no fixture yet.
    /// `AccountUpdateReason::merge` itself is a four-line pure function with its wire values pinned
    /// in `perp-core`.
    #[test]
    fn a_liquidation_that_closes_nothing_charges_no_fee_and_reports_one_cause() {
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
        assert_eq!(
            position(&mut ctx, ALICE).amount,
            QTY,
            "fixture: nothing may close — empty book, insolvent residual, no ADL counterparty"
        );
        assert_eq!(
            storage::load_insurance_fund(&mut ctx).unwrap(),
            0,
            "a liquidation that consumed no margin charges no clearance fee, however many times \
             it is retried — this is what stops a never-ADL-able position from having its wallet \
             drained one fee per oracle tick forever"
        );
        let reasons = account_update_reasons(&logs);
        assert_eq!(
            reasons
                .iter()
                .filter(|(u, _)| *u == ALICE)
                .map(|(_, r)| *r)
                .collect::<Vec<_>>(),
            // EXACTLY ONE group: the funding settle, published inline. No clearance fee (nothing
            // closed), and the post-funding position write's mark is cleared because the drain
            // would only repeat that same payload with no position rows.
            vec![R::FundingFee],
            "got {reasons:?}"
        );
    }
}

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
mod liquidation_cap {
    use super::*;

    /// 55 identical 5x longs, each its own address so each is its own registry entry. Seeded in a
    /// deterministic order so the test can also speak about WHICH ones get done.
    /// `losers` underwater longs of `QTY`, against `QTY * losers` counterparties holding ONE unit
    /// each.
    ///
    /// The shape matters: one fill per counterparty means a single loser costs `QTY` fills, so the
    /// shared 128-fill ADL budget runs out after ~12 liquidations — long before the 50-slot cap.
    /// A shared whale would absorb each loser in ONE fill and the slot cap would bind first, which
    /// is a different test.
    pub(super) fn seed_underwater_longs_fragmented(ctx: &mut TestCtx, losers: u8) -> Vec<Address> {
        let users: Vec<Address> = (1..=losers)
            .map(|i| {
                let user = Address::from([i; 20]);
                seed_position_account(ctx, user, QTY, -ENTRY_VALUE, MARGIN, 5, 0);
                user
            })
            .collect();
        for k in 0..(losers as u16) * (QTY as u16) {
            let mut b = [0xC0u8; 20];
            b[18] = (k >> 8) as u8;
            b[19] = k as u8;
            // One unit, same $100 entry and same 5x as the longs, so it is deeply in profit at any
            // mark below $100 and stays an eligible candidate throughout.
            seed_position_account(ctx, Address::from(b), -1, ENTRY_VALUE / QTY, MARGIN / QTY, 5, 0);
        }
        users
    }

    pub(super) fn seed_underwater_longs(ctx: &mut TestCtx, n: u8) -> Vec<Address> {
        let users: Vec<Address> = (1..=n)
            .map(|i| {
                let user = Address::from([i; 20]);
                seed_position_account(ctx, user, QTY, -ENTRY_VALUE, MARGIN, 5, 0);
                user
            })
            .collect();
        // LAST, so the losers keep registry indices 0..n and the order-sensitive test below still
        // reads straightforwardly. Sized to absorb every one of them: without a counterparty each
        // liquidation would defer instead of completing, and these tests would be measuring
        // deferral rather than the cap.
        seed_absorbing_short(ctx, n as i64);
        users
    }

    pub(super) fn still_open(ctx: &mut TestCtx, users: &[Address]) -> usize {
        users
            .iter()
            .filter(|u| storage::load_position(ctx, **u, MARKET_ID).unwrap().amount != 0)
            .count()
    }

    /// ONE update liquidates exactly 50 and leaves the rest untouched; a SECOND update finishes
    /// them. The deferral is the documented behaviour and it had no coverage.
    #[test]
    fn the_cap_defers_the_overflow_to_the_next_update() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let users = seed_underwater_longs(&mut ctx, 55);
        assert_eq!(still_open(&mut ctx, &users), 55);

        // $100 -> $85: every one of them is below maintenance but still solvent.
        oracle_update_at(&mut ctx, 8_500, 31);
        assert_eq!(
            still_open(&mut ctx, &users),
            5,
            "exactly MAX_SWEEP_ACTIONS_PER_UPDATE (50) liquidated in one update"
        );

        // A second update at the SAME price — the mark does not have to move again. This is the
        // part worth pinning: nothing was stored as pending, the registry is simply re-scanned.
        oracle_update_at(&mut ctx, 8_500, 61);
        assert_eq!(still_open(&mut ctx, &users), 0, "the overflow is drained, not lost");
    }

    /// ⚠️ WHICH 50 is POSITIONAL, not severity-ranked: the sweep snapshots the registry and walks
    /// it in INSERTION order from index 0 every update, with no cursor and no rotation. Pinned
    /// because it is a fairness property nothing else states, and because any future rotation /
    /// severity ordering must consciously break this test rather than silently change behaviour.
    #[test]
    fn the_first_fifty_by_registry_order_are_the_ones_liquidated() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let users = seed_underwater_longs(&mut ctx, 55);
        let mut expected = users.clone();
        expected.push(WHALE); // seeded last by `seed_underwater_longs`
        assert_eq!(
            storage::load_position_registry(&mut ctx, MARKET_ID).unwrap(),
            expected,
            "registry is insertion-ordered — the premise of this test"
        );

        oracle_update_at(&mut ctx, 8_500, 31);

        for (i, u) in users.iter().enumerate() {
            let open = storage::load_position(&mut ctx, *u, MARKET_ID).unwrap().amount != 0;
            assert_eq!(
                open,
                i >= 50,
                "registry index {i} should {} be open",
                if i >= 50 { "" } else { "NOT" }
            );
        }
    }

    /// Under the cap, nothing is deferred — guards against a future change that defers work it did
    /// not need to, which the test above could not distinguish from correct behaviour.
    #[test]
    fn under_the_cap_one_update_clears_everything() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let users = seed_underwater_longs(&mut ctx, 49);
        oracle_update_at(&mut ctx, 8_500, 31);
        assert_eq!(still_open(&mut ctx, &users), 0);
    }

    pub(super) fn oracle_update_at(ctx: &mut TestCtx, index: u64, ts: u64) {
        run_update_index_price(
            &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index, timestamp: ts }
                .abi_encode(),
            ADMIN,
            ctx,
        )
        .unwrap();
    }
}

/// `drainDeferredWork` — the permissionless drain for work a capped sweep left behind.
/// The algo-order trigger index. Its SORT DIRECTIONS are load-bearing, not cosmetic: the sweep
/// walks each list from the front and stops at the first price that does not fire, which is what
/// makes the scan cost proportional to how far the mark moved instead of to how many algo orders
/// exist. Get a direction backwards and the sweep either fires nothing or walks everything.
/// `placeAlgoOrder` admission. Every rejection here is a rejection Binance would also make, or a
/// deliberate, documented divergence — there are no "we did not get round to it" errors.
mod algo_order_admission {
    use super::*;
    use crate::interface::IPerpDex::{cancelAlgoOrderCall, placeAlgoOrderCall};
    use crate::types::AlgoOrderType;

    const RO: u8 = 1; // reduceOnly
    const CP: u8 = 2; // closePosition

    #[allow(clippy::too_many_arguments)]
    fn place(
        ctx: &mut TestCtx,
        side: Side,
        kind: AlgoOrderType,
        trigger: u64,
        price: u64,
        qty: u64,
        tif: u8,
        working_type: u8,
        flags: u8,
    ) -> Result<Bytes, PerpError> {
        crate::algo::run_place_algo_order(
            &placeAlgoOrderCall {
                algoType: 0,
                marketId: MARKET_ID,
                side: side as u8,
                orderType: kind as u8,
                triggerPrice: trigger,
                price,
                quantity: qty,
                tif,
                workingType: working_type,
                clientAlgoId: FixedBytes::default(),
                flags,
            }
            .abi_encode(),
            ALICE,
            ctx,
        )
    }

    /// A reduce-only STOP_MARKET — the canonical stop-loss — placed, listed and cancelled.
    #[test]
    fn a_stop_market_round_trips() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let out =
            place(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 8_000, 0, QTY as u64, 0, 0, RO)
                .unwrap();
        let algo_id: [u8; 32] = out[..32].try_into().unwrap();

        assert_eq!(
            storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap(),
            vec![algo_id]
        );
        // A SELL stop fires when the mark FALLS to the trigger, so it lives in the down index.
        assert_eq!(
            storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap(),
            vec![8_000]
        );
        assert!(
            storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
            "and nowhere near the up index"
        );
        // ⚠️ It must NOT be in the book: an untriggered conditional has no price level, no queue
        // position and no open-order requirement.
        assert!(storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
        assert!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap().is_empty());

        crate::algo::run_cancel_algo_order(
            &cancelAlgoOrderCall { algoId: FixedBytes(algo_id), marketId: MARKET_ID }.abi_encode(),
            ALICE,
            &mut ctx,
        )
        .unwrap();
        assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
        assert!(storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
        assert!(storage::load_order(&mut ctx, &algo_id).unwrap().is_none());
    }

    /// The trigger-direction matrix — the one thing a reader cannot verify by inspection, because
    /// four (type, side) pairs collapse onto two index directions.
    #[test]
    fn all_four_type_side_pairs_land_in_the_right_direction() {
        for (kind, side, up) in [
            (AlgoOrderType::StopMarket, Side::Buy, true),
            (AlgoOrderType::StopMarket, Side::Sell, false),
            (AlgoOrderType::TakeProfitMarket, Side::Buy, false),
            (AlgoOrderType::TakeProfitMarket, Side::Sell, true),
        ] {
            let mut ctx = make_ctx();
            setup_market(&mut ctx);
            place(&mut ctx, side, kind, 8_000, 0, QTY as u64, 0, 0, RO).unwrap();
            let in_up = !storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap().is_empty();
            assert_eq!(
                in_up, up,
                "{kind:?} {side:?} must fire on mark {} trigger",
                if up { ">=" } else { "<=" }
            );
        }
    }

    /// ⚠️ The deliberate divergence, asserted so it cannot be softened into a silent substitution:
    /// Binance's DEFAULT workingType is CONTRACT_PRICE, so a client ported from there which omits
    /// the field lands here, and it must be TOLD rather than quietly triggered off the mark.
    #[test]
    fn contract_price_working_type_is_rejected_not_substituted() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let e = place(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 8_000, 0, QTY as u64, 0, 1, RO)
            .unwrap_err();
        assert!(format!("{e:?}").contains("MARK_PRICE"), "got {e:?}");
    }

    #[test]
    fn rejections() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let bad = |ctx: &mut TestCtx, c: placeAlgoOrderCall, want: &str| {
            let e = crate::algo::run_place_algo_order(&c.abi_encode(), ALICE, ctx).unwrap_err();
            assert!(format!("{e:?}").contains(want), "want {want:?}, got {e:?}");
        };
        let base = placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: Side::Sell as u8,
            orderType: AlgoOrderType::StopMarket as u8,
            triggerPrice: 8_000,
            price: 0,
            quantity: QTY as u64,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: RO,
        };
        bad(&mut ctx, placeAlgoOrderCall { algoType: 1, ..base.clone() }, "CONDITIONAL");
        // TRAILING_STOP_MARKET is roadmap 1d; rejected by name so the gap is legible.
        bad(&mut ctx, placeAlgoOrderCall { orderType: 4, ..base.clone() }, "orderType");
        bad(&mut ctx, placeAlgoOrderCall { triggerPrice: 0, ..base.clone() }, "triggerPrice");
        bad(&mut ctx, placeAlgoOrderCall { flags: 0, ..base.clone() }, "reduceOnly or closePosition");
        bad(&mut ctx, placeAlgoOrderCall { flags: 0b1000, ..base.clone() }, "unknown flag");
        // closePosition is *_MARKET only, and excludes an explicit quantity.
        bad(
            &mut ctx,
            placeAlgoOrderCall { orderType: AlgoOrderType::Stop as u8, price: 8_000, flags: CP, ..base.clone() },
            "closePosition requires",
        );
        bad(&mut ctx, placeAlgoOrderCall { flags: CP, ..base.clone() }, "exclusive");
        bad(&mut ctx, placeAlgoOrderCall { quantity: 0, ..base.clone() }, "quantity must be non-zero");
        // A *_MARKET type carries no resting price and no tif.
        bad(&mut ctx, placeAlgoOrderCall { price: 8_000, ..base.clone() }, "price must be 0");
        bad(&mut ctx, placeAlgoOrderCall { tif: 1, ..base.clone() }, "tif is unused");
        // A limit type must carry one.
        bad(
            &mut ctx,
            placeAlgoOrderCall { orderType: AlgoOrderType::Stop as u8, price: 0, ..base.clone() },
            "price is required",
        );
    }

    /// The per-market cap is not cosmetic: liquidation's and ADL's cancel-all walk this list from
    /// inside an already-capped sweep, so an unbounded list would put unbounded work there.
    #[test]
    fn the_per_market_cap_is_enforced() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        for i in 0..crate::algo::MAX_ALGO_ORDERS_PER_MARKET {
            place(
                &mut ctx,
                Side::Sell,
                AlgoOrderType::StopMarket,
                8_000 + (i as u64 + 1) * 100,
                0,
                QTY as u64,
                0,
                0,
                RO,
            )
            .unwrap();
        }
        let e = place(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 7_000, 0, QTY as u64, 0, 0, RO)
            .unwrap_err();
        assert!(format!("{e:?}").contains("too many open conditional"), "got {e:?}");
    }

    /// Liquidation and ADL both clear a market's book for the owner. A conditional order is NOT in
    /// the book, so it has to be cleared explicitly — otherwise a liquidated account keeps a live
    /// stop-loss aimed at a position that no longer exists, which would fire against whatever it
    /// opens next.
    #[test]
    fn liquidation_cancels_conditional_orders_too() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
        seed_absorbing_short(&mut ctx, 1);
        place(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 8_000, 0, QTY as u64, 0, 0, RO)
            .unwrap();
        assert_eq!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(), 1);

        oracle_tick(&mut ctx, 7_500, 31);

        assert_eq!(position(&mut ctx, ALICE).amount, 0, "fixture: ALICE really was liquidated");
        assert!(
            storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty(),
            "her conditional orders must go with the position"
        );
        assert!(
            storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
            "and leave the market trigger index, not just her own list"
        );
    }
}

/// The trigger sweep: when a conditional order fires, what it becomes, and — the part that is a
/// deliberate divergence from Binance — what happens when it does NOT get to fire.
mod algo_trigger_sweep {
    use super::*;
    use crate::interface::IPerpDex::placeAlgoOrderCall;
    use crate::types::AlgoOrderType;

    const RO: u8 = 1;
    const CP: u8 = 2;

    fn stop_market(ctx: &mut TestCtx, user: Address, trigger: u64, qty: u64) -> [u8; 32] {
        let out = crate::algo::run_place_algo_order(
            &placeAlgoOrderCall {
                algoType: 0,
                marketId: MARKET_ID,
                side: Side::Sell as u8,
                orderType: AlgoOrderType::StopMarket as u8,
                triggerPrice: trigger,
                price: 0,
                quantity: qty,
                tif: 0,
                workingType: 0,
                clientAlgoId: FixedBytes::default(),
                flags: RO,
            }
            .abi_encode(),
            user,
            ctx,
        )
        .unwrap();
        out[..32].try_into().unwrap()
    }

    /// A stop-loss fires on the mark and closes the position against the book.
    #[test]
    fn a_stop_market_fires_and_closes_against_the_book() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
        // Liquidity to close into, inside the fill-time band around the $95 mark.
        place_order(&mut ctx, MAKER, Side::Buy as u8, 9_400, QTY as u64);
        let id = stop_market(&mut ctx, ALICE, 9_500, QTY as u64);

        // $100 -> $95: at the trigger, and ALICE is still comfortably above maintenance, so this
        // is the stop firing rather than a liquidation.
        oracle_tick(&mut ctx, 9_500, 31);

        assert_eq!(position(&mut ctx, ALICE).amount, 0, "the stop closed the position");
        assert!(
            storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty(),
            "and left the conditional namespace"
        );
        assert!(storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
        assert!(
            storage::load_order(&mut ctx, &id).unwrap().is_none(),
            "fully filled market order: delete-on-terminal, same as any other"
        );
    }

    /// Above the trigger nothing happens — the sweep must not fire early.
    #[test]
    fn it_does_not_fire_before_the_mark_reaches_the_trigger() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
        let id = stop_market(&mut ctx, ALICE, 9_000, QTY as u64);

        // ⚠️ $99, not $95. At $95 this 5x long is already BELOW maintenance (equity $150 against a
        // ~$158 requirement at mmr = 1/6) and the liquidation sweep closes it — which would make
        // this test pass for entirely the wrong reason the day the trigger logic broke.
        oracle_tick(&mut ctx, 9_900, 31);

        assert_eq!(position(&mut ctx, ALICE).amount, QTY, "mark 9900 > trigger 9000");
        assert!(storage::load_order(&mut ctx, &id).unwrap().is_some());
        assert_eq!(storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap(), vec![9_000]);
    }

    /// ⚠️ THE DIVERGENCE FROM BINANCE, and the reason it is safe.
    ///
    /// Binance LATCHES: once the trigger price is touched the order becomes a normal order and a
    /// retrace cannot undo that. We do not latch — the sweep re-derives from the CURRENT mark
    /// every run — so a trigger the budget could not reach does not fire later at a price that no
    /// longer exists. Same rule the fill-time band imposes: something declined to execute it, so
    /// it did not execute.
    ///
    /// Here the position is closed by other means between the two ticks, which is the cleanest way
    /// to observe "the condition is re-checked rather than remembered": the order fires, finds
    /// nothing to close, and expires instead of opening a short.
    #[test]
    fn a_trigger_whose_position_vanished_expires_instead_of_opening() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
        let id = stop_market(&mut ctx, ALICE, 9_500, QTY as u64);

        // Close the position out from under the conditional order, by hand.
        let mut pos = storage::load_position(&mut ctx, ALICE, MARKET_ID).unwrap();
        pos.amount = 0;
        pos.v_quote_balance = 0;
        pos.margin = 0;
        storage::save_position(&mut ctx, ALICE, MARKET_ID, &pos, AccountUpdateReason::Adjustment)
            .unwrap();

        oracle_tick(&mut ctx, 9_500, 31);

        assert_eq!(
            position(&mut ctx, ALICE).amount,
            0,
            "a reduce-only stop must NEVER open exposure — this is the whole point of the flag"
        );
        assert!(storage::load_order(&mut ctx, &id).unwrap().is_none(), "expired");
        assert!(storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
    }

    #[allow(clippy::too_many_arguments)]
    fn place_algo(
        ctx: &mut TestCtx,
        user: Address,
        side: Side,
        kind: AlgoOrderType,
        trigger: u64,
        price: u64,
        qty: u64,
        flags: u8,
    ) -> [u8; 32] {
        let out = crate::algo::run_place_algo_order(
            &placeAlgoOrderCall {
                algoType: 0,
                marketId: MARKET_ID,
                side: side as u8,
                orderType: kind as u8,
                triggerPrice: trigger,
                price,
                quantity: qty,
                tif: if kind.is_market() { 0 } else { 0 }, // GTC for the limit pair
                workingType: 0,
                clientAlgoId: FixedBytes::default(),
                flags,
            }
            .abi_encode(),
            user,
            ctx,
        )
        .unwrap();
        out[..32].try_into().unwrap()
    }

    /// **The LIMIT variant's trigger path, which had no test.** Every trigger test used
    /// `STOP_MARKET`; `AlgoOrderType::Stop` appeared only in two REJECTION assertions, so the
    /// branch that hands a fired order to `place_order_core` as a resting LIMIT had never run.
    ///
    /// This is also the half of the Binance parity pair that does NOT expire: a market stop says
    /// "now or not at all", a limit stop says "at this price or better, and keep waiting".
    #[test]
    fn a_limit_stop_rests_in_the_book_when_it_fires() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
        // No liquidity on purpose: the fired order must REST, which is the thing being pinned.
        let id = place_algo(&mut ctx, ALICE, Side::Sell, AlgoOrderType::Stop, 9_900, 9_950, QTY as u64, RO);

        // ⚠️ 9_900, not 9_500. At 9_500 this 5x long is already BELOW maintenance (equity $150
        // against a ~$158 requirement), so the liquidation sweep closes it and cancels every order
        // she holds — including the one just rested. The test would then be measuring liquidation.
        oracle_tick(&mut ctx, 9_900, 31);

        assert_eq!(
            storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(),
            vec![9_950],
            "the fired limit order must be RESTING in the book at its own price"
        );
        let o = storage::load_order(&mut ctx, &id).unwrap().expect("still live");
        assert_eq!(o.status, OrderStatus::Open);
        assert_eq!(o.trigger_price, 0, "it is an ordinary order now, not a conditional one");
        assert_eq!(
            position(&mut ctx, ALICE).amount,
            QTY,
            "nothing matched it, so the position is untouched — it is waiting, not failed"
        );
        assert!(
            storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty(),
            "and it left the conditional namespace on the way"
        );
    }

    /// A trigger is never refused by the per-market open-order cap, even when its owner sits AT
    /// the cap: `fire` takes the conditional off the owner's list before placing it, so the fired
    /// limit order converts one counted open order into another. Were it refused, the refusal
    /// would land inside the ORACLE transaction.
    ///
    /// ⚠️ Mutation note: "check the cap on the trigger path too" is an EQUIVALENT mutation — by the
    /// argument above the count a trigger sees is at most 199, so the check would pass. What this
    /// pins is the outcome (rests, no error, still exactly at the cap), and the `Triggered` arm's
    /// `debug_assert!` pins the invariant that makes the skip safe.
    #[test]
    fn a_trigger_at_the_open_order_cap_still_rests() {
        use crate::algo::MAX_OPEN_ORDERS_PER_MARKET;
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, 100_000_000_000);
        let id = place_algo(&mut ctx, ALICE, Side::Sell, AlgoOrderType::Stop, 9_900, 9_950, QTY as u64, RO);
        // 199 resting bids, inside the fill-time band so the oracle tick's band expiry leaves them
        // alone, and below the stop's price so the fired sell does not cross them.
        for _ in 0..MAX_OPEN_ORDERS_PER_MARKET - 1 {
            place_order(&mut ctx, ALICE, Side::Buy as u8, 9_000, 1);
        }
        let open = |ctx: &mut TestCtx| {
            storage::book_order_count(ctx, ALICE, MARKET_ID).unwrap()
                + storage::load_user_algo_orders(ctx, ALICE, MARKET_ID).unwrap().len()
        };
        assert_eq!(open(&mut ctx), MAX_OPEN_ORDERS_PER_MARKET, "fixture: exactly at the cap");
        let e = try_place_order(&mut ctx, ALICE, Side::Buy as u8, 9_000, 1).unwrap_err();
        assert!(format!("{e:?}").contains("max open orders reached"), "got {e:?}");

        oracle_tick(&mut ctx, 9_900, 31);

        assert_eq!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(), vec![9_950]);
        assert_eq!(
            storage::load_order(&mut ctx, &id).unwrap().expect("rested").status,
            OrderStatus::Open
        );
        assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
        assert_eq!(open(&mut ctx), MAX_OPEN_ORDERS_PER_MARKET, "one counted order became another");
    }

    /// **`closePosition` reads the position AT TRIGGER TIME, which had no test.** The flag only
    /// appeared in rejection assertions, so the branch that substitutes `|position|` for the
    /// (necessarily zero) `quantity` had never run.
    ///
    /// The fixture grows the position between arming and firing, which is what makes it
    /// discriminating twice over: a bug using the stored `quantity` would close 0 and EXPIRE the
    /// order, and a bug snapshotting the size at PLACEMENT would close only half.
    #[test]
    fn close_position_takes_the_size_the_position_has_when_it_fires() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(
            &mut ctx, ALICE, QTY / 2, -ENTRY_VALUE / 2, MARGIN / 2, 5, USER_WALLET as i64,
        );
        // quantity = 0 — mandatory with closePosition, and exactly what a naive implementation
        // would then try to close.
        place_algo(&mut ctx, ALICE, Side::Sell, AlgoOrderType::StopMarket, 9_500, 0, 0, RO | CP);

        // Grow it AFTER arming.
        seed_position_account(
            &mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64,
        );
        place_order(&mut ctx, MAKER, Side::Buy as u8, 9_400, QTY as u64);

        oracle_tick(&mut ctx, 9_500, 31);

        assert_eq!(
            position(&mut ctx, ALICE).amount,
            0,
            "the WHOLE position at trigger time, not the half it was armed against and not zero"
        );
    }

    /// **The band-blocked remainder EXPIRES, which I asserted in a commit message and never
    /// tested.** The claim was that routing a fired `_MARKET` order through the ordinary path
    /// gives Binance's "unfilled market order expires" for free, with no special case. Free or
    /// not, it was unverified.
    ///
    /// Liquidity exists here — it is simply OUTSIDE the fill-time band around the post-drop mark,
    /// which is precisely the fast-move shape where a stop is most wanted and least fillable.
    #[test]
    fn a_market_stop_the_band_blocks_expires_rather_than_resting() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
        // The band is +/-10% of the mark, so at a 9_500 mark it is [8_550, 10_450]. A bid at 7_000
        // is real liquidity the band will not let the close reach.
        place_order(&mut ctx, MAKER, Side::Buy as u8, 7_000, QTY as u64);
        let id = place_algo(&mut ctx, ALICE, Side::Sell, AlgoOrderType::StopMarket, 9_500, 0, QTY as u64, RO);

        oracle_tick(&mut ctx, 9_500, 31);

        assert_eq!(
            position(&mut ctx, ALICE).amount,
            QTY,
            "nothing in band to close into, so nothing closed"
        );
        assert!(
            storage::load_order(&mut ctx, &id).unwrap().is_none(),
            "EXPIRED, not resting: a market order means now or not at all (Binance parity)"
        );
        assert!(
            storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
            "and it must NOT have become a resting sell — that would be a different order type"
        );
        assert!(
            storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty(),
            "the conditional is spent either way; the owner must re-arm"
        );
    }

    /// **The budget-exhaustion path, which had no test at all.** `run_trigger_sweep`'s early
    /// return is its ONLY deferral route, and until now nothing exercised it — the cleanup that
    /// removed a dead `deferred` local could just as easily have removed the `return Ok(true)`
    /// and every test would still have passed.
    ///
    /// The fixture is deliberately cheap: these stops have no position to protect, so each one
    /// fires, finds zero capacity and EXPIRES. That still costs one unit of budget apiece, which
    /// is exactly what is being measured — the budget meters trigger EXECUTIONS, not fills.
    ///
    /// It also pins the hand-off to the drain: hitting the budget must set `deferred_work`, or a
    /// keeper has no way to learn there is anything left to finish.
    #[test]
    fn exhausting_the_sweep_budget_defers_the_rest_and_arms_the_drain() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let n = crate::risk::MAX_SWEEP_ACTIONS_PER_UPDATE + 5;
        for i in 1..=n {
            let user = Address::from([i as u8; 20]);
            stop_market(&mut ctx, user, 9_500, QTY as u64);
        }
        assert_eq!(
            storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap(),
            vec![9_500],
            "all of them share one trigger price, so this is one level with n entries"
        );

        oracle_tick(&mut ctx, 9_500, 31);

        let left = storage::load_algo_level(&mut ctx, MARKET_ID, false, 9_500).unwrap().len();
        assert_eq!(
            left as u32,
            n - crate::risk::MAX_SWEEP_ACTIONS_PER_UPDATE,
            "exactly the budget's worth fired; the remainder is still indexed"
        );
        assert_eq!(
            storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap().deferred_work,
            1,
            "a budget stopped us, not the market — the drain must be told"
        );

        // The drain gets a fresh budget and finishes them, in the SAME block and therefore at the
        // SAME mark that fired them — which is what keeps not-latching safe.
        crate::risk::run_drain_deferred_work(
            &drainDeferredWorkCall { marketId: MARKET_ID }.abi_encode(),
            &mut ctx,
        )
        .unwrap();
        assert!(
            storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
            "the drain cleared the backlog"
        );
    }

    /// Triggers are served BEFORE liquidations out of the shared budget. Not politeness: a stop
    /// that executes removes the position the liquidation sweep would otherwise have had to
    /// handle, so spending on triggers reduces what is left to spend on liquidations.
    #[test]
    fn a_stop_fires_rather_than_the_owner_being_liquidated() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
        // In-band liquidity at the $85 mark.
        place_order(&mut ctx, MAKER, Side::Buy as u8, 8_400, QTY as u64);
        stop_market(&mut ctx, ALICE, 8_500, QTY as u64);

        // $85 is below maintenance for this 5x long, so WITHOUT the stop the sweep would
        // liquidate her. The stop runs first and closes it as an ordinary trade instead.
        oracle_tick(&mut ctx, 8_500, 31);

        assert_eq!(position(&mut ctx, ALICE).amount, 0);
        let liq = JournalTr::take_logs(ctx.journal_mut())
            .into_iter()
            .filter(|l| {
                l.data.topics().first()
                    == Some(&crate::interface::IPerpDex::Liquidation::SIGNATURE_HASH)
            })
            .count();
        assert_eq!(liq, 0, "closed by her own stop, not liquidated");
    }
}

/// The signed (API-key) path for conditional orders — the one real traders use, since the direct
/// call needs an EOA with gas and a nonce. Binance's analogue is `X-MBX-APIKEY` plus an HMAC over
/// the query string; here `keyId` names the registered ed25519 slot and the signature covers a
/// fixed-layout canonical message.
mod algo_order_signed {
    use super::*;
    use crate::interface::IPerpDex::placeAlgoOrderSignedCall;
    use crate::types::{AlgoOrderType, ApiKey};
    use ed25519_dalek::{Signer, SigningKey};

    const RO: u8 = 1;
    const TS: u64 = 1;
    const RECV: u64 = 60;

    fn register(ctx: &mut TestCtx, user: Address, sk: &SigningKey) {
        storage::save_api_key(
            ctx,
            user,
            0,
            ApiKey { pubkey: sk.verifying_key().to_bytes(), expiry: 0 },
        )
        .unwrap();
    }

    /// Rebuilds the canonical message INDEPENDENTLY of the engine. If the two ever disagree the
    /// signature stops verifying, which is the point — a helper that called the engine's own
    /// writer would pin nothing.
    fn msg(chain_id: u64, account: Address, trigger: u64, qty: u64) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(b"perpdex_v1_algo");
        m.extend_from_slice(&chain_id.to_be_bytes());
        m.extend_from_slice(account.as_slice());
        m.push(0); // algoType = CONDITIONAL
        m.extend_from_slice(&MARKET_ID.to_be_bytes());
        m.push(Side::Sell as u8);
        m.push(AlgoOrderType::StopMarket as u8);
        m.extend_from_slice(&trigger.to_be_bytes());
        m.extend_from_slice(&0u64.to_be_bytes()); // price
        m.extend_from_slice(&qty.to_be_bytes());
        m.push(0); // tif
        m.push(0); // workingType = MARK_PRICE
        m.extend_from_slice(&[0u8; 16]); // clientAlgoId
        m.push(RO);
        m.extend_from_slice(&TS.to_be_bytes());
        m.extend_from_slice(&RECV.to_be_bytes());
        m.push(0); // keyId
        m
    }

    fn calldata(sig: &[u8], trigger: u64, qty: u64, account: Address) -> Vec<u8> {
        placeAlgoOrderSignedCall {
            account,
            algoType: 0,
            marketId: MARKET_ID,
            side: Side::Sell as u8,
            orderType: AlgoOrderType::StopMarket as u8,
            triggerPrice: trigger,
            price: 0,
            quantity: qty,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: RO,
            timestamp: TS,
            recvWindow: RECV,
            keyId: 0,
            signature: sig.to_vec().into(),
        }
        .abi_encode()
    }

    #[test]
    fn a_relayer_can_submit_a_signed_conditional_order() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register(&mut ctx, ALICE, &sk);
        let chain_id = ctx.chain_id();
        let sig = sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes();

        // Submitted by someone who is NOT the owner — that is the whole point of the signed path.
        crate::algo::run_place_algo_order_signed(&calldata(&sig, 8_000, QTY as u64, ALICE), &mut ctx)
            .unwrap();

        assert_eq!(
            storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(),
            1,
            "the order belongs to the SIGNER, not the sender"
        );
    }

    /// ⚠️ The cross-chain replay guard. The same signature on a chain with a different id must not
    /// verify — this is what stops a message captured on one deployment being replayed onto
    /// another, and it is the reason `chainId` is inside the signed bytes rather than merely
    /// checked alongside them.
    #[test]
    fn a_signature_from_another_chain_does_not_verify() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register(&mut ctx, ALICE, &sk);
        let other_chain = ctx.chain_id() ^ 1;
        let sig = sk.sign(&msg(other_chain, ALICE, 8_000, QTY as u64)).to_bytes();

        let e = crate::algo::run_place_algo_order_signed(
            &calldata(&sig, 8_000, QTY as u64, ALICE),
            &mut ctx,
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("signature"), "got {e:?}");
        assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
    }

    /// Tampering with a field the signature covers must fail. `quantity` is the sharpest one: a
    /// relayer that could inflate it would be placing an order the owner never authorised.
    #[test]
    fn tampering_with_the_wire_quantity_does_not_verify() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register(&mut ctx, ALICE, &sk);
        let chain_id = ctx.chain_id();
        let sig = sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes();

        let e = crate::algo::run_place_algo_order_signed(
            &calldata(&sig, 8_000, QTY as u64 * 2, ALICE),
            &mut ctx,
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("signature"), "got {e:?}");
    }

    // ── cancelAlgoOrderSigned ────────────────────────────────────────────────────────────────
    //
    // ⚠️ This entrypoint shipped with ZERO coverage — its `debug_assert_eq!` on the canonical
    // message length had never once been executed. It is a signed path anyone may relay, so the
    // gap mattered more than its size suggested.

    /// Rebuilt INDEPENDENTLY of the engine, same reason as the place-side helper: a helper that
    /// called the engine's own writer would pin nothing.
    fn cancel_msg(chain_id: u64, account: Address, algo_id: [u8; 32]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(b"perpdex_v1_algocancel");
        m.extend_from_slice(&chain_id.to_be_bytes());
        m.extend_from_slice(account.as_slice());
        m.extend_from_slice(&algo_id);
        m.extend_from_slice(&MARKET_ID.to_be_bytes());
        m.extend_from_slice(&TS.to_be_bytes());
        m.extend_from_slice(&RECV.to_be_bytes());
        m.push(0); // keyId
        m
    }

    fn cancel_calldata(sig: &[u8], account: Address, algo_id: [u8; 32]) -> Vec<u8> {
        crate::interface::IPerpDex::cancelAlgoOrderSignedCall {
            account,
            algoId: FixedBytes(algo_id),
            marketId: MARKET_ID,
            timestamp: TS,
            recvWindow: RECV,
            keyId: 0,
            signature: sig.to_vec().into(),
        }
        .abi_encode()
    }

    /// Places one, then cancels it over the signed path with a third party relaying.
    #[test]
    fn a_relayer_can_cancel_a_signed_conditional_order() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register(&mut ctx, ALICE, &sk);
        let chain_id = ctx.chain_id();
        let place_sig = sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes();
        let out = crate::algo::run_place_algo_order_signed(
            &calldata(&place_sig, 8_000, QTY as u64, ALICE),
            &mut ctx,
        )
        .unwrap();
        let algo_id: [u8; 32] = out[..32].try_into().unwrap();

        let sig = sk.sign(&cancel_msg(chain_id, ALICE, algo_id)).to_bytes();
        crate::algo::run_cancel_algo_order_signed(&cancel_calldata(&sig, ALICE, algo_id), &mut ctx)
            .unwrap();

        assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
        assert!(
            storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
            "and it left the market trigger index, not just her own list"
        );
        assert!(storage::load_order(&mut ctx, &algo_id).unwrap().is_none());
    }

    /// ⚠️ The sharpest tamper on this entrypoint: `algoId` is what says WHICH order dies. A relayer
    /// able to swap it could cancel any order the owner holds — a signature authorising the cancel
    /// of one stop would become authority to strip them all.
    ///
    /// It signs for one REAL order and submits against another REAL one, so a pass means the
    /// AIMED-AT order survived rather than merely that some error was returned.
    ///
    /// ⚠️ What this test does NOT catch, measured rather than assumed: deleting `algoId` from the
    /// engine's signed message entirely. No independently-built-message test can — the helper
    /// still includes the field, so the two messages differ either way and verification fails for
    /// a reason unrelated to tampering. The mutation that removes the field is caught by the
    /// HAPPY-PATH tests above, which need a verify to SUCCEED. Do not read a green here as proof
    /// the field is covered; read the pair.
    #[test]
    fn tampering_with_the_cancelled_algo_id_does_not_verify() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register(&mut ctx, ALICE, &sk);
        let chain_id = ctx.chain_id();

        let place = |ctx: &mut TestCtx, trigger: u64| -> [u8; 32] {
            let sig = sk.sign(&msg(chain_id, ALICE, trigger, QTY as u64)).to_bytes();
            let out = crate::algo::run_place_algo_order_signed(
                &calldata(&sig, trigger, QTY as u64, ALICE),
                ctx,
            )
            .unwrap();
            out[..32].try_into().unwrap()
        };
        let signed_for = place(&mut ctx, 8_000);
        let victim = place(&mut ctx, 7_000);
        assert_ne!(signed_for, victim);

        // Authority to cancel `signed_for`, aimed at `victim`.
        let sig = sk.sign(&cancel_msg(chain_id, ALICE, signed_for)).to_bytes();
        let e = crate::algo::run_cancel_algo_order_signed(
            &cancel_calldata(&sig, ALICE, victim),
            &mut ctx,
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("signature"), "got {e:?}");
        assert_eq!(
            storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(),
            2,
            "BOTH orders must survive — neither the aimed-at one nor the authorised one"
        );
    }

    #[test]
    fn a_cancel_signature_from_another_chain_does_not_verify() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register(&mut ctx, ALICE, &sk);
        let chain_id = ctx.chain_id();
        let a = crate::algo::run_place_algo_order_signed(
            &calldata(&sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes(), 8_000, QTY as u64, ALICE),
            &mut ctx,
        )
        .unwrap();
        let algo_id: [u8; 32] = a[..32].try_into().unwrap();

        let sig = sk.sign(&cancel_msg(chain_id ^ 1, ALICE, algo_id)).to_bytes();
        let e = crate::algo::run_cancel_algo_order_signed(
            &cancel_calldata(&sig, ALICE, algo_id),
            &mut ctx,
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("signature"), "got {e:?}");
        assert_eq!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(), 1);
    }

    /// A replayed signed cancel is refused as a DUPLICATE SIGNATURE: the signature is burned on
    /// submission, as on every other signed selector. A FRESH signature for the same, already
    /// cancelled order still finds nothing (the order is deleted).
    #[test]
    fn replaying_a_cancel_is_refused_as_a_duplicate_signature() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register(&mut ctx, ALICE, &sk);
        let chain_id = ctx.chain_id();
        let a = crate::algo::run_place_algo_order_signed(
            &calldata(&sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes(), 8_000, QTY as u64, ALICE),
            &mut ctx,
        )
        .unwrap();
        let algo_id: [u8; 32] = a[..32].try_into().unwrap();
        let sig = sk.sign(&cancel_msg(chain_id, ALICE, algo_id)).to_bytes();
        let data = cancel_calldata(&sig, ALICE, algo_id);

        crate::algo::run_cancel_algo_order_signed(&data, &mut ctx).unwrap();
        let e = crate::algo::run_cancel_algo_order_signed(&data, &mut ctx).unwrap_err();
        assert!(format!("{e:?}").contains("duplicate signature"), "got {e:?}");

        // A different, valid signature over the same cancel (one second later, still inside the
        // recv window) is not a replay — it reaches the cancel and finds the order already gone.
        let mut m = cancel_msg(chain_id, ALICE, algo_id);
        let ts_at = m.len() - 1 - 8 - 8; // …‖timestamp(8)‖recvWindow(8)‖keyId(1)
        m[ts_at..ts_at + 8].copy_from_slice(&(TS + 1).to_be_bytes());
        let fresh = crate::interface::IPerpDex::cancelAlgoOrderSignedCall {
            account: ALICE,
            algoId: FixedBytes(algo_id),
            marketId: MARKET_ID,
            timestamp: TS + 1,
            recvWindow: RECV,
            keyId: 0,
            signature: sk.sign(&m).to_bytes().to_vec().into(),
        }
        .abi_encode();
        let e = crate::algo::run_cancel_algo_order_signed(&fresh, &mut ctx).unwrap_err();
        assert!(format!("{e:?}").contains("not found"), "got {e:?}");
    }

    /// Replay: one signature, one order. The id IS `keccak256(signature)`, and the same hash keys
    /// the seen-signature set, so a resubmission bounces off the guard rather than placing a
    /// second order.
    #[test]
    fn the_same_signature_cannot_be_submitted_twice() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register(&mut ctx, ALICE, &sk);
        let chain_id = ctx.chain_id();
        let sig = sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes();
        let data = calldata(&sig, 8_000, QTY as u64, ALICE);

        crate::algo::run_place_algo_order_signed(&data, &mut ctx).unwrap();
        let e = crate::algo::run_place_algo_order_signed(&data, &mut ctx).unwrap_err();
        assert!(format!("{e:?}").contains("duplicate signature"), "got {e:?}");
        assert_eq!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(), 1);
    }
}

mod algo_trigger_index {
    use super::*;

    fn id(n: u8) -> [u8; 32] {
        [n; 32]
    }

    #[test]
    fn up_prices_are_ascending_and_down_prices_descending() {
        let mut ctx = make_ctx();
        for (p, n) in [(300u64, 1u8), (100, 2), (200, 3)] {
            storage::algo_index_insert(&mut ctx, MARKET_ID, true, p, id(n)).unwrap();
            storage::algo_index_insert(&mut ctx, MARKET_ID, false, p, id(n)).unwrap();
        }
        assert_eq!(
            storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap(),
            vec![100, 200, 300],
            "up list ASC — fires on mark >= trigger, so the LOWEST trigger fires first"
        );
        assert_eq!(
            storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap(),
            vec![300, 200, 100],
            "down list DESC — fires on mark <= trigger, so the HIGHEST trigger fires first"
        );
    }

    #[test]
    fn a_price_leaves_the_sorted_list_only_when_its_level_empties() {
        let mut ctx = make_ctx();
        storage::algo_index_insert(&mut ctx, MARKET_ID, true, 100, id(1)).unwrap();
        storage::algo_index_insert(&mut ctx, MARKET_ID, true, 100, id(2)).unwrap();
        assert_eq!(storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap(), vec![100]);

        storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(1)).unwrap();
        assert_eq!(
            storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap(),
            vec![100],
            "one order left at this price — the price must stay, or the sweep would skip it"
        );
        assert_eq!(storage::load_algo_level(&mut ctx, MARKET_ID, true, 100).unwrap(), vec![id(2)]);

        storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(2)).unwrap();
        assert!(
            storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
            "level empty — the price must go, or the sweep walks a price with nothing behind it"
        );
    }

    #[test]
    fn removing_an_unindexed_order_is_a_no_op() {
        let mut ctx = make_ctx();
        storage::algo_index_insert(&mut ctx, MARKET_ID, true, 100, id(1)).unwrap();
        // Idempotent on purpose: cancel, liquidation cancel-all and the sweep can all reach the
        // same order, and the second remover must not corrupt the price list.
        storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(9)).unwrap();
        storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(1)).unwrap();
        storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(1)).unwrap();
        assert!(storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
    }

    #[test]
    fn the_two_directions_are_independent_at_the_same_price() {
        let mut ctx = make_ctx();
        storage::algo_index_insert(&mut ctx, MARKET_ID, true, 100, id(1)).unwrap();
        storage::algo_index_insert(&mut ctx, MARKET_ID, false, 100, id(2)).unwrap();
        storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(1)).unwrap();
        assert!(storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
        assert_eq!(
            storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap(),
            vec![100],
            "a STOP and a TAKE_PROFIT can share a trigger price and must not evict each other"
        );
    }

    #[test]
    fn orders_at_one_trigger_price_keep_placement_order() {
        let mut ctx = make_ctx();
        for n in [5u8, 3, 9] {
            storage::algo_index_insert(&mut ctx, MARKET_ID, true, 100, id(n)).unwrap();
        }
        assert_eq!(
            storage::load_algo_level(&mut ctx, MARKET_ID, true, 100).unwrap(),
            vec![id(5), id(3), id(9)],
            "FIFO by placement, mirroring the book's level queue"
        );
    }
}

mod deferred_work_drain {
    use super::liquidation_cap::*;
    use super::*;

    /// A stranger with no role. Goes through the real dispatcher so the gas entry and the
    /// non-static classification are exercised too, not just the handler. Returns `moreRemaining`.
    const STRANGER: Address = Address::new([0xDD; 20]);

    fn drain(ctx: &mut TestCtx) -> bool {
        let out = run_perp_dex_call(
            &drainDeferredWorkCall { marketId: MARKET_ID }.abi_encode(),
            1_000_000,
            STRANGER,
            U256::ZERO,
            false,
            ctx,
        )
        .unwrap();
        assert!(!out.reverted, "drainDeferredWork reverted: {:?}", out.bytes);
        drainDeferredWorkCall::abi_decode_returns(&out.bytes).unwrap()
    }

    fn flag(ctx: &mut TestCtx) -> u8 {
        storage::load_market(ctx, MARKET_ID).unwrap().unwrap().deferred_work
    }

    /// The whole point, end to end: the cap defers, the bit records it, a stranger drains it in the
    /// SAME block, and the bit clears. Without this the backlog waits for the next oracle tick.
    #[test]
    fn a_stranger_drains_the_overflow_in_the_same_block() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let users = seed_underwater_longs(&mut ctx, 55);

        oracle_update_at(&mut ctx, 8_500, 31);
        assert_eq!(still_open(&mut ctx, &users), 5, "the cap deferred 5");
        assert_eq!(flag(&mut ctx), 1, "and recorded that it did");

        // No role, no oracle, no waiting for `priceUpdateInterval`.
        assert!(!drain(&mut ctx), "the backlog is gone, so nothing more remains");
        assert_eq!(still_open(&mut ctx, &users), 0);
        assert_eq!(flag(&mut ctx), 0, "the bit clears on the way out");
    }

    /// `moreRemaining` is a usable loop condition: 105 underwater positions need the oracle tick
    /// plus two drains, and the return value says exactly that without the caller reading state.
    #[test]
    fn more_remaining_drives_the_keeper_loop() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let users = seed_underwater_longs(&mut ctx, 105);

        oracle_update_at(&mut ctx, 8_500, 31);
        assert_eq!(still_open(&mut ctx, &users), 55);

        assert!(drain(&mut ctx), "50 more done, 5 still queued");
        assert_eq!(still_open(&mut ctx, &users), 5);
        assert_eq!(flag(&mut ctx), 1);

        assert!(!drain(&mut ctx));
        assert_eq!(still_open(&mut ctx, &users), 0);
        assert_eq!(flag(&mut ctx), 0);
    }

    /// A poll on a market whose bit is `0` is READ-ONLY and does no sweep — it returns on the bit
    /// alone, before the registry walk and before the book walk. That is what makes "call it every
    /// block" affordable, and it is also what bounds the permissionless surface: the drain finishes
    /// work a cap deferred, it is NOT a second liquidation sweep anyone can trigger off-tick.
    ///
    /// The positions here are genuinely below maintenance at the stored mark with the bit still
    /// `0` — the documented false-negative. A drain that skipped the early return would liquidate
    /// them and this would fail on both counts.
    #[test]
    fn a_poll_with_nothing_deferred_is_read_only_and_sweeps_nothing() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let users = seed_underwater_longs(&mut ctx, 3);
        // Move the mark WITHOUT an oracle tick, so nothing clears them and the bit stays 0.
        storage::save_mark_price(&mut ctx, MARKET_ID, 8_500).unwrap();
        assert_eq!(flag(&mut ctx), 0, "no sweep has run, so nothing has been deferred");

        let before = ctx.perp_write_count();
        assert!(!drain(&mut ctx));
        assert_eq!(ctx.perp_write_count(), before, "a no-op poll is read-only");
        assert_eq!(still_open(&mut ctx, &users), 3, "and it swept nothing");

        // Again, to catch a first-call-only memoisation.
        assert!(!drain(&mut ctx));
        assert_eq!(ctx.perp_write_count(), before);

        // The oracle tick is what clears them — the drain never substitutes for it.
        oracle_update_at(&mut ctx, 8_500, 31);
        assert_eq!(still_open(&mut ctx, &users), 0);
    }

    /// The drain does the sweeps and NOTHING else. Re-running the oracle tail between ticks would
    /// fold a second premium sample into the epoch and move `nextFundingTime` — i.e. a
    /// permissionless call would be able to skew the funding rate. Pinned against exactly that.
    #[test]
    fn the_drain_does_not_touch_the_price_or_the_funding_epoch() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let users = seed_underwater_longs(&mut ctx, 55);
        oracle_update_at(&mut ctx, 8_500, 31);

        let mark = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap().mark_price;
        let index = storage::load_index_price_state(&mut ctx, MARKET_ID).unwrap().index_price;
        let funding = storage::load_funding_state(&mut ctx, MARKET_ID).unwrap();
        let acc = storage::load_premium_accumulator(&mut ctx, MARKET_ID).unwrap();

        drain(&mut ctx);
        assert_eq!(still_open(&mut ctx, &users), 0, "it did do the work");

        assert_eq!(
            storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap().mark_price,
            mark,
            "no mark recompute"
        );
        assert_eq!(
            storage::load_index_price_state(&mut ctx, MARKET_ID).unwrap().index_price,
            index,
            "no index write"
        );
        assert_eq!(
            storage::load_funding_state(&mut ctx, MARKET_ID).unwrap(),
            funding,
            "no funding settlement, no epoch roll"
        );
        assert_eq!(
            storage::load_premium_accumulator(&mut ctx, MARKET_ID).unwrap(),
            acc,
            "no extra premium sample — a permissionless rate skew"
        );
    }

    /// **The ADL fill budget reaches the drain.** The fixture is built so the SLOT cap provably
    /// cannot be the thing that fires: 20 losers is well under 50, but each needs `QTY` separate
    /// counterparties, so the shared 128-fill budget runs out around the 13th liquidation.
    ///
    /// Before this signal existed the sweep returned `Ok(false)` whenever it completed its walk,
    /// so an ADL-starved backlog left the bit at `0` and `drainDeferredWork` returned on the fast
    /// path without sweeping anything — the whole backlog waited for the next oracle tick. This
    /// pins that it no longer does.
    #[test]
    fn the_adl_fill_budget_also_sets_the_bit() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        // 20 losers x QTY fills each = 200 fills needed against only 20 slots: the 128-fill ADL
        // budget is provably what stops this, not the 50-slot cap.
        let users = seed_underwater_longs_fragmented(&mut ctx, 20);

        oracle_update_at(&mut ctx, 8_500, 31);
        let open_after_tick = still_open(&mut ctx, &users);
        assert!(open_after_tick > 0, "the fixture must leave a backlog");
        assert!(
            20 - open_after_tick < MAX_SWEEP_ACTIONS_PER_UPDATE as usize,
            "fixture: the slot cap must NOT be what stopped this, or the test proves nothing"
        );
        assert_eq!(flag(&mut ctx), 1, "a budget, not the market, stopped it");

        // The drain gets fresh budgets and makes real progress — the thing the bit buys.
        drain(&mut ctx);
        assert!(
            still_open(&mut ctx, &users) < open_after_tick,
            "the drain must actually reduce the backlog"
        );
    }

    #[test]
    fn an_unknown_market_reverts() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let out = run_perp_dex_call(
            &drainDeferredWorkCall { marketId: MARKET_ID + 7 }.abi_encode(),
            1_000_000,
            STRANGER,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(out.reverted, "an unknown market must revert, not silently return false");
    }
}

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
mod withdrawal_cap {
    use super::*;
    use crate::events::stream_test_support::{account_update_reasons, stream_shape};
    use crate::interface::IPerpDex::{
        getPositionRiskCall, AccountPosition, FundingSettled, Liquidation, PositionChanged,
        PositionMarginAdjusted, Trade,
    };
    use crate::types::AccountUpdateReason as R;

    const G1: &str = "removePositionMargin: insufficient position margin";
    const G2: &str = "removePositionMargin: resulting margin below maintenance margin";
    const G3: &str = "removePositionMargin: resulting equity below initial margin requirement";
    const G4: &str = "removePositionMargin: resulting position below maintenance margin";
    const LIQ_REFUSED: &str = "liquidate: position is above maintenance margin";

    /// `q = 3` on the base-0 / price-2 grid: `n(p) = p · 3e4`, divisible by 1, 2, 3 and 6 at every
    /// cent price, so every cap identity below holds to the micro-unit.
    const Q: i64 = 3;
    /// Notional of `Q` at `ENTRY_PRICE`: `10_000 · 3 · 1e6 / 1e2`.
    const N0: i64 = 300_000_000;
    const TAKER_BPS: u64 = 10;
    /// 10 bps of `N0` — only ever compared against the fee READ off the `Trade` event.
    const OPEN_FEE: u64 = 300_000;
    /// ALICE's perp wallet before a real-fill open.
    const FUNDS: i64 = 2_000_000_000;
    /// A second market on a fractional grid (`price_decimals = 8`): `n = price · qty / 100`, so the
    /// notional is truncated and `PIM` rounds up for real — the property test's second half.
    const FRAC_MARKET: u64 = 2;
    const FRAC_PRICE_DECIMALS: u32 = 8;

    // ── read side: the client's view ─────────────────────────────────────────────────────────

    fn risk_row_on(ctx: &mut TestCtx, user: Address, market_id: u64) -> AccountPosition {
        let out = crate::margin_view::run_get_position_risk(
            &getPositionRiskCall {
                user,
                marketId: market_id,
            }
            .abi_encode(),
            ctx,
        )
        .unwrap();
        let row = getPositionRiskCall::abi_decode_returns(&out).unwrap();
        assert_eq!(
            row.isolatedMargin as i128,
            row.isolatedWallet as i128 + row.unrealizedProfit as i128,
            "isolatedMargin is isolatedWallet + unrealizedProfit"
        );
        row
    }

    fn risk_row(ctx: &mut TestCtx) -> AccountPosition {
        risk_row_on(ctx, ALICE, MARKET_ID)
    }

    /// The cap a client computes from `getPositionRisk` alone — never from the gate:
    /// `max(0, min(isolatedWallet − maintMargin, isolatedMargin − max(positionInitialMargin,
    /// maintMargin)))`. Valid only with no funding pending (`getPositionRisk` does not settle).
    fn cap_of(row: &AccountPosition) -> u64 {
        let iw = row.isolatedWallet as i128;
        let mm = row.maintMargin as i128;
        let pim = row.positionInitialMargin as i128;
        let equity = row.isolatedMargin as i128;
        u64::try_from((iw - mm).min(equity - pim.max(mm)).max(0)).unwrap()
    }

    /// HEAD's rule before 2b, on the same row: `amount ≤ iw ∧ iw − amount + uPnL ≥ MM`.
    fn old_rule_cap(row: &AccountPosition) -> u64 {
        let iw = row.isolatedWallet as i128;
        let u = row.unrealizedProfit as i128;
        let mm = row.maintMargin as i128;
        u64::try_from(iw.min(iw + u - mm).max(0)).unwrap()
    }

    /// The gate a request of `amount` must fail, read off the row in the documented order
    /// (G1 → G4); `None` when it clears all four.
    fn gate_for(row: &AccountPosition, amount: u64) -> Option<&'static str> {
        let a = amount as i128;
        let iw = row.isolatedWallet as i128;
        let u = row.unrealizedProfit as i128;
        let pim = row.positionInitialMargin as i128;
        let mm = row.maintMargin as i128;
        if a > iw {
            Some(G1)
        } else if iw - a < mm {
            Some(G2)
        } else if iw - a + u < pim {
            Some(G3)
        } else if iw - a + u < mm {
            Some(G4)
        } else {
            None
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum Regime {
        /// `uPnL < PIM − MM`: the equity term `iw + uPnL − PIM` is the smaller one (spec "第二项").
        Equity,
        /// `uPnL > PIM − MM`: the balance term `iw − MM` is the smaller one (spec "第一项").
        Balance,
        /// `MM > PIM ∧ uPnL < 0`: G4's `iw + uPnL − MM` is below both — the safety net.
        SafetyNet,
    }

    fn regime_of(row: &AccountPosition) -> Option<Regime> {
        let u = row.unrealizedProfit as i128;
        let pim = row.positionInitialMargin as i128;
        let mm = row.maintMargin as i128;
        if mm > pim && u < 0 {
            Some(Regime::SafetyNet)
        } else if u < pim - mm {
            Some(Regime::Equity)
        } else if u > pim - mm {
            Some(Regime::Balance)
        } else {
            None
        }
    }

    fn assert_regime(row: &AccountPosition, want: Regime) {
        assert_eq!(
            regime_of(row),
            Some(want),
            "fixture regime: uPnL {} PIM {} MM {}",
            row.unrealizedProfit,
            row.positionInitialMargin,
            row.maintMargin
        );
    }

    // ── write side: the probes ───────────────────────────────────────────────────────────────

    #[derive(Debug, PartialEq)]
    struct Snapshot {
        pos: PerpPosition,
        account: UserAccount,
        insurance_fund: u64,
        perp_writes: u64,
    }

    fn snapshot_on(ctx: &mut TestCtx, user: Address, market_id: u64) -> Snapshot {
        Snapshot {
            pos: storage::load_position(ctx, user, market_id).unwrap(),
            account: storage::load_account(ctx, user).unwrap(),
            insurance_fund: storage::load_insurance_fund(ctx).unwrap(),
            perp_writes: ctx.perp_write_count(),
        }
    }

    fn decode<E: SolEvent>(logs: &[primitives::Log]) -> Vec<E> {
        logs.iter()
            .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
            .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
            .collect()
    }

    fn remove_on(
        ctx: &mut TestCtx,
        user: Address,
        market_id: u64,
        amount: u64,
    ) -> Result<Bytes, PerpError> {
        run_remove_position_margin(
            &removePositionMarginCall {
                marketId: market_id,
                amount,
            }
            .abi_encode(),
            user,
            ctx,
        )
    }

    /// Sends `amount` and asserts it is refused WHOLE, by exactly `gate`: position (every stored
    /// field, `last_funding_index` included), account, insurance fund and the perp write counter
    /// bit-identical, and not one log — no `FundingSettled`, no `PositionMarginAdjusted`, no
    /// `PositionChanged`, no `AccountBalanceChanged`. Nothing truncated, nothing partially taken,
    /// no funding settled on the way out.
    fn assert_refused_whole_on(
        ctx: &mut TestCtx,
        user: Address,
        market_id: u64,
        amount: u64,
        gate: &str,
    ) {
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let before = snapshot_on(ctx, user, market_id);
        let err = remove_on(ctx, user, market_id, amount).unwrap_err();
        assert!(
            matches!(&err, PerpError::Reject(m) if m == gate),
            "removePositionMargin({amount}) must be refused by `{gate}`, got `{err}`"
        );
        assert_eq!(
            snapshot_on(ctx, user, market_id),
            before,
            "a refused removePositionMargin({amount}) must leave everything bit-identical"
        );
        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert!(
            logs.is_empty(),
            "a refused removePositionMargin({amount}) emitted {:?}",
            stream_shape(&logs)
        );
    }

    fn assert_refused_whole(ctx: &mut TestCtx, amount: u64, gate: &str) {
        assert_refused_whole_on(ctx, ALICE, MARKET_ID, amount, gate);
    }

    /// Sends `amount` and asserts it is honoured EXACTLY: margin −amount, wallet +amount, nothing
    /// else about the position moves, and one `PositionMarginAdjusted(delta = −amount)`. Only for
    /// states with no funding pending (a settle would move the margin and the index too).
    fn assert_withdrawn_exactly_on(ctx: &mut TestCtx, user: Address, market_id: u64, amount: u64) {
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let pos0 = storage::load_position(ctx, user, market_id).unwrap();
        let wallet0 = storage::load_account(ctx, user).unwrap().perp_wallet_balance;
        remove_on(ctx, user, market_id, amount).unwrap();
        let pos1 = storage::load_position(ctx, user, market_id).unwrap();
        let wallet1 = storage::load_account(ctx, user).unwrap().perp_wallet_balance;
        let mut want = pos0.clone();
        want.margin = pos0.margin - amount as i64;
        assert_eq!(pos1, want, "only the margin moves, and by exactly {amount}");
        assert_eq!(wallet1, wallet0 + amount as i64, "the wallet gets exactly {amount}");
        let logs = JournalTr::take_logs(ctx.journal_mut());
        let adjusted = decode::<PositionMarginAdjusted>(&logs);
        assert_eq!(adjusted.len(), 1);
        assert_eq!(
            (
                adjusted[0].user,
                adjusted[0].marketId,
                adjusted[0].delta,
                adjusted[0].margin
            ),
            (user, market_id, -(amount as i64), pos1.margin)
        );
    }

    /// THE BOUNDARY PROBE. `cap + 1` first — refused whole by `over_cap_gate` (and the row itself
    /// must name that gate) — then `cap`, honoured exactly. With `cap == 0` only the `1` is sent.
    /// Afterwards the cap is 0 and the binding term sits exactly on its bound. Returns the cap.
    fn probe_on(ctx: &mut TestCtx, user: Address, market_id: u64, over_cap_gate: &str) -> u64 {
        let row = risk_row_on(ctx, user, market_id);
        let cap = cap_of(&row);
        if cap > 0 {
            assert_eq!(gate_for(&row, cap), None, "the cap {cap} itself clears every gate");
        }
        assert_eq!(
            gate_for(&row, cap + 1),
            Some(over_cap_gate),
            "fixture: the row names a different gate for cap + 1 = {}",
            cap + 1
        );
        assert_refused_whole_on(ctx, user, market_id, cap + 1, over_cap_gate);
        if cap > 0 {
            assert_withdrawn_exactly_on(ctx, user, market_id, cap);
            let after = risk_row_on(ctx, user, market_id);
            assert_eq!(cap_of(&after), 0, "taking the cap leaves nothing withdrawable");
            assert!(after.isolatedWallet as i128 >= after.maintMargin as i128);
            assert!(
                after.isolatedMargin as i128
                    >= after.positionInitialMargin.max(after.maintMargin) as i128
            );
        }
        cap
    }

    fn probe(ctx: &mut TestCtx, over_cap_gate: &str) -> u64 {
        probe_on(ctx, ALICE, MARKET_ID, over_cap_gate)
    }

    /// T9: `liquidate()` at the same mark is refused, and the refusal writes nothing.
    fn assert_not_liquidatable(ctx: &mut TestCtx) {
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let before = snapshot_on(ctx, ALICE, MARKET_ID);
        let err = liquidate(ctx, ALICE).unwrap_err();
        assert!(
            matches!(&err, PerpError::Reject(m) if m == LIQ_REFUSED),
            "a position at the cap must not be liquidatable at the same mark: {err}"
        );
        assert_eq!(snapshot_on(ctx, ALICE, MARKET_ID), before);
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
    }

    // ── fixtures ─────────────────────────────────────────────────────────────────────────────

    /// A fresh fixture market with ALICE holding a SYNTHETIC position at `mark`, no funding
    /// pending — for the tests where exact control of margin / vQuote / leverage matters more
    /// than the route there.
    fn seeded(amount: i64, v_quote: i64, margin: i64, leverage: u64, mark: u64) -> TestCtx {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(
            &mut ctx,
            ALICE,
            amount,
            v_quote,
            margin,
            leverage,
            USER_WALLET as i64,
        );
        storage::save_mark_price(&mut ctx, MARKET_ID, mark).unwrap();
        ctx
    }

    /// Opens ALICE `q` contracts at `ENTRY_PRICE` through a REAL matched fill against MAKER, at
    /// leverage `lev` and taker fee `taker_bps`. Returns the opening fee as READ off the `Trade`
    /// event, cross-checked against the silo: `floor(value / L) − fee`.
    fn open_via_fill(ctx: &mut TestCtx, long: bool, q: u64, lev: u64, taker_bps: u64) -> u64 {
        storage::save_account(
            ctx,
            ALICE,
            UserAccount {
                perp_wallet_balance: FUNDS,
                ..UserAccount::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
        storage::save_user_fee_rates(
            ctx,
            ALICE,
            UserFeeRates {
                maker_fee_bps: 0,
                taker_fee_bps: taker_bps,
            },
        )
        .unwrap();
        set_leverage(ctx, lev).unwrap();
        let (maker_side, taker_side) = if long {
            (Side::Sell, Side::Buy)
        } else {
            (Side::Buy, Side::Sell)
        };
        place_maker_order(ctx, maker_side as u8, ENTRY_PRICE, q);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        try_place_order(ctx, ALICE, taker_side as u8, ENTRY_PRICE, q).unwrap();
        let trades = decode::<Trade>(&JournalTr::take_logs(ctx.journal_mut()));
        assert_eq!(trades.len(), 1);
        assert_eq!(
            (trades[0].taker, trades[0].price, trades[0].quantity),
            (ALICE, ENTRY_PRICE, q)
        );
        let fee = trades[0].takerFee;
        let value = calc_value(ENTRY_PRICE, q, 0, PRICE_DECIMALS).unwrap() as i64;
        let pos = position(ctx, ALICE);
        assert_eq!(pos.amount, if long { q as i64 } else { -(q as i64) });
        assert_eq!(pos.v_quote_balance, if long { -value } else { value });
        assert_eq!(pos.leverage, lev);
        assert_eq!(
            pos.margin,
            value / lev as i64 - fee as i64,
            "the opening fee comes OUT of the silo"
        );
        fee
    }

    fn opened(long: bool, q: u64, lev: u64, taker_bps: u64) -> (TestCtx, u64) {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let fee = open_via_fill(&mut ctx, long, q, lev, taker_bps);
        (ctx, fee)
    }

    fn set_mark(ctx: &mut TestCtx, mark: u64) {
        storage::save_mark_price(ctx, MARKET_ID, mark).unwrap();
    }

    // ── T1 / T2 / T2b / T3 / T4: the two terms on both sides of the mark ─────────────────────

    /// T1 (+ T9): a LONG in profit, the EQUITY term binding. Real fill (10 bps), `addPositionMargin(E)`
    /// with `E` = the opening margin, mark `1.10·entry` (`1.20·entry` at `L = 1`). Closed form:
    /// `cap = E − fee + (n₁ − n₀)(1 − 1/L)`. After taking it the silo sits BELOW `PIM` while equity
    /// sits exactly ON it, and the position is not liquidatable.
    #[test]
    fn t1_long_in_profit_withdraws_down_to_equity_equal_to_pim() {
        for (lev, e, mark, want) in [
            (1u64, 300_000_000u64, 12_000u64, 299_700_000u64),
            (2, 150_000_000, 11_000, 164_700_000),
            (3, 100_000_000, 11_000, 119_700_000),
        ] {
            let (mut ctx, fee) = opened(true, Q as u64, lev, TAKER_BPS);
            assert_eq!(fee, OPEN_FEE);
            add_position_margin(&mut ctx, e).unwrap();
            set_mark(&mut ctx, mark);
            let row = risk_row(&mut ctx);
            assert_regime(&row, Regime::Equity);
            let n1 = mark as i64 * Q * 10_000;
            assert_eq!(
                want as i64,
                e as i64 - fee as i64 + (n1 - N0) - (n1 - N0) / lev as i64,
                "L = {lev}: closed form"
            );
            assert_eq!(cap_of(&row), want, "L = {lev}");
            assert_eq!(probe(&mut ctx, G3), want, "L = {lev}");

            let after = risk_row(&mut ctx);
            assert!(
                after.isolatedWallet < after.positionInitialMargin as i64,
                "L = {lev}: the balance ends BELOW PIM, carried by unrealized profit"
            );
            assert_eq!(
                after.isolatedMargin,
                after.positionInitialMargin as i64,
                "L = {lev}: equity lands exactly ON PIM"
            );
            if lev == 3 {
                assert!(after.positionInitialMargin >= 2 * after.maintMargin, "PIM ≥ 2·MM");
            }
            assert_not_liquidatable(&mut ctx);
        }
    }

    /// `old + 1`, one past the OLD rule's cap `iw + uPnL − MM` on a losing position: the margin
    /// left, `MM − uPnL − 1`, still clears G2, while equity lands on `MM − 1` — below `MM` and below
    /// `PIM ≥ MM`. G3 and G4 both fail, and G3, the earlier gate, must be the one named.
    fn assert_g3_named_before_g4(ctx: &mut TestCtx, row: &AccountPosition, old: u64) {
        let over = old as i128 + 1;
        assert!(row.unrealizedProfit < 0 && row.positionInitialMargin >= row.maintMargin);
        assert!(
            row.isolatedWallet as i128 - over >= row.maintMargin as i128,
            "old + 1 = {over} leaves the margin ≥ MM: G2 passes"
        );
        assert_eq!(
            row.isolatedMargin as i128 - over,
            row.maintMargin as i128 - 1,
            "old + 1 = {over} leaves equity at MM − 1: G3 and G4 both fail"
        );
        assert_refused_whole(ctx, old + 1, G3);
    }

    /// T2 [extrapolated cell]: a LONG at a loss, mark `0.90·entry`. Closed form
    /// `cap = M − n₀ + n₁ − ⌈n₁/L⌉`; the over-cap request fails G3, and so does the OLD rule's own
    /// cap (`min(iw, iw + uPnL − MM)`), which HEAD would have honoured — the loss-side tightening.
    /// One past the old cap fails G3 AND G4 with G2 passing, which pins G3 ahead of G4.
    #[test]
    fn t2_long_at_a_loss_is_capped_by_the_equity_term() {
        for (lev, margin, want, old) in [
            (1u64, 600_000_000i64, 300_000_000u64, 525_000_000u64),
            (2, 300_000_000, 135_000_000, 225_000_000),
            (3, 200_000_000, 80_000_000, 125_000_000),
        ] {
            let mut ctx = seeded(Q, -N0, margin, lev, 9_000);
            let row = risk_row(&mut ctx);
            assert_regime(&row, Regime::Equity);
            assert_eq!(row.unrealizedProfit, -30_000_000);
            assert_eq!(
                want as i64,
                margin - N0 + 270_000_000 - 270_000_000 / lev as i64,
                "L = {lev}: closed form"
            );
            assert_eq!(cap_of(&row), want, "L = {lev}");
            assert_eq!(old_rule_cap(&row), old, "L = {lev}");
            assert!(old > want);
            assert_g3_named_before_g4(&mut ctx, &row, old);
            assert_refused_whole(&mut ctx, old, G3);
            assert_eq!(probe(&mut ctx, G3), want, "L = {lev}");
        }
    }

    /// T2b: a loss-side state where the OLD rule accepts and 2b refuses with the G3 string. Long 10
    /// @ $100, `L = 3`, margin $400, mark $95: `N = 950M`, `uPnL = −50M`, `PIM = ⌈950M/3⌉ =
    /// 316_666_667`, `MM = 158_333_333`. New cap `min(241_666_667, 33_333_333) = 33_333_333`; old
    /// cap `min(400M, 191_666_667)`. Removing $100 clears both of the old rule's checks on these
    /// very fields, and 2b refuses it on the equity term.
    #[test]
    fn t2b_loss_side_old_rule_accepts_and_2b_refuses_on_equity() {
        let mut ctx = seeded(QTY, -ENTRY_VALUE, 400_000_000, 3, 9_500);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::Equity);
        assert_eq!(row.notional, 950_000_000);
        assert_eq!(row.unrealizedProfit, -50_000_000);
        assert_eq!(row.positionInitialMargin, 316_666_667);
        assert_eq!(row.maintMargin, 158_333_333);
        assert_eq!(cap_of(&row), 33_333_333);
        assert_eq!(old_rule_cap(&row), 191_666_667);

        let amount = 100_000_000u64;
        let (iw, u, mm) = (
            row.isolatedWallet as i128,
            row.unrealizedProfit as i128,
            row.maintMargin as i128,
        );
        assert!(
            amount as i128 <= iw && iw - amount as i128 + u >= mm,
            "the old rule's two checks both pass"
        );
        assert_refused_whole(&mut ctx, amount, G3);
        assert_eq!(probe(&mut ctx, G3), 33_333_333);
    }

    /// T3 (+ T9): a SHORT in profit releases its `PIM` WITH the mark, at slope `1 + 1/L` — marks
    /// `0.95·entry` then `0.90·entry`, each probed from an identical fresh state (same `iw`, same
    /// `vQuote`, no funding). `q(m₁ − m₂) = 15_000_000` µ, untruncated, and `n(m₁) ≡ n(m₂) (mod L)`,
    /// so the slope is exact to the unit. The cap also exceeds `isolatedMargin − ⌈|vQuote|/L⌉`,
    /// which rules out "PIM at the worse of entry and mark". The LONG mirror (`1.05`, `1.10`) runs at
    /// slope `1 − 1/L` (`0` at `L = 1`: a ×1 long's cap does not move with the mark).
    #[test]
    fn t3_profit_releases_pim_with_the_mark_at_the_exact_slope() {
        let dq: i64 = 15_000_000;
        for (lev, margin, want) in [
            (1u64, 400_000_000i64, [130_000_000u64, 160_000_000u64]),
            (2, 300_000_000, [172_500_000, 195_000_000]),
            (3, 200_000_000, [120_000_000, 140_000_000]),
        ] {
            let mut caps = [0u64; 2];
            for (i, mark) in [9_500u64, 9_000].into_iter().enumerate() {
                let mut ctx = seeded(-Q, N0, margin, lev, mark);
                let row = risk_row(&mut ctx);
                assert_regime(&row, Regime::Equity);
                let n = mark as i64 * Q * 10_000;
                assert_eq!(
                    want[i] as i64,
                    margin + N0 - n - n / lev as i64,
                    "short L = {lev} @ {mark}: closed form"
                );
                let worse_of = row.isolatedMargin as i128
                    - (row.vQuoteBalance.unsigned_abs() as u128).div_ceil(lev as u128) as i128;
                assert!(
                    cap_of(&row) as i128 > worse_of,
                    "short L = {lev} @ {mark}: PIM is released with the mark"
                );
                caps[i] = probe(&mut ctx, G3);
                assert_eq!(caps[i], want[i], "short L = {lev} @ {mark}");
                if lev == 3 {
                    let after = risk_row(&mut ctx);
                    assert!(after.positionInitialMargin >= 2 * after.maintMargin, "PIM ≥ 2·MM");
                }
                assert_not_liquidatable(&mut ctx);
            }
            assert_eq!(
                caps[1] as i64 - caps[0] as i64,
                dq + dq / lev as i64,
                "short L = {lev}: q·(m₁ − m₂)·(1 + 1/L)"
            );
        }
        for (lev, margin, want) in [
            (1u64, 600_000_000i64, [300_000_000u64, 300_000_000u64]),
            (2, 300_000_000, [157_500_000, 165_000_000]),
            (3, 200_000_000, [110_000_000, 120_000_000]),
        ] {
            let mut caps = [0u64; 2];
            for (i, mark) in [10_500u64, 11_000].into_iter().enumerate() {
                let mut ctx = seeded(Q, -N0, margin, lev, mark);
                assert_regime(&risk_row(&mut ctx), Regime::Equity);
                caps[i] = probe(&mut ctx, G3);
                assert_eq!(caps[i], want[i], "long L = {lev} @ {mark}");
                assert_not_liquidatable(&mut ctx);
            }
            assert_eq!(
                caps[1] as i64 - caps[0] as i64,
                dq - dq / lev as i64,
                "long L = {lev}: q·(m₂ − m₁)·(1 − 1/L)"
            );
        }
    }

    /// T4: a SHORT at a loss, mark `1.10·entry` — T2's mirror. Closed form
    /// `cap = M + |vQuote| − n₁ − ⌈n₁/L⌉`; the old rule's cap is refused by G3, and so is one past
    /// it, where G4 fails too.
    #[test]
    fn t4_short_at_a_loss_is_capped_by_the_equity_term() {
        for (lev, margin, want, old) in [
            (1u64, 600_000_000i64, 240_000_000u64, 515_000_000u64),
            (2, 300_000_000, 105_000_000, 215_000_000),
            (3, 200_000_000, 60_000_000, 115_000_000),
        ] {
            let mut ctx = seeded(-Q, N0, margin, lev, 11_000);
            let row = risk_row(&mut ctx);
            assert_regime(&row, Regime::Equity);
            assert_eq!(row.unrealizedProfit, -30_000_000);
            assert_eq!(
                want as i64,
                margin + N0 - 330_000_000 - 330_000_000 / lev as i64,
                "L = {lev}: closed form"
            );
            assert_eq!(cap_of(&row), want, "L = {lev}");
            assert_eq!(old_rule_cap(&row), old, "L = {lev}");
            assert_g3_named_before_g4(&mut ctx, &row, old);
            assert_refused_whole(&mut ctx, old, G3);
            assert_eq!(probe(&mut ctx, G3), want, "L = {lev}");
        }
    }

    // ── T5 / T14: the fresh open ─────────────────────────────────────────────────────────────

    /// T5: a ×1 LONG through a real fill. Its equity term is exactly `iw + vQuote`, independent of
    /// the mark. ① no margin added: `−fee` at 0.9 / 1.0 / 1.2 × entry, 1 µ refused by G3 each time;
    /// ② `addPositionMargin(E)`: the cap is `E − fee` at all three marks (fee as READ off the fill);
    /// ③ a zero-fee open: equity sits exactly on `PIM`, the cap is exactly 0 and 1 µ is refused.
    #[test]
    fn t5_a_1x_long_cap_is_e_minus_the_opening_fee_at_every_mark() {
        const MARKS: [u64; 3] = [9_000, 10_000, 12_000];
        const E: u64 = 300_000_000;

        let (mut ctx, fee) = opened(true, Q as u64, 1, TAKER_BPS);
        assert_eq!(fee, OPEN_FEE);
        for mark in MARKS {
            set_mark(&mut ctx, mark);
            let row = risk_row(&mut ctx);
            assert_regime(&row, Regime::Equity);
            assert_eq!(
                row.isolatedMargin - row.positionInitialMargin as i64,
                -(fee as i64),
                "@ {mark}: the ×1 long's equity term is iw + vQuote = −fee"
            );
            assert_eq!(probe(&mut ctx, G3), 0, "@ {mark}");
        }

        for mark in MARKS {
            let (mut ctx, fee) = opened(true, Q as u64, 1, TAKER_BPS);
            add_position_margin(&mut ctx, E).unwrap();
            set_mark(&mut ctx, mark);
            assert_regime(&risk_row(&mut ctx), Regime::Equity);
            assert_eq!(probe(&mut ctx, G3), E - fee, "@ {mark}");
            assert_eq!(E - fee, 299_700_000);
        }

        for mark in MARKS {
            let (mut ctx, fee) = opened(true, Q as u64, 1, 0);
            assert_eq!(fee, 0);
            set_mark(&mut ctx, mark);
            let row = risk_row(&mut ctx);
            assert_regime(&row, Regime::Equity);
            assert_eq!(row.isolatedMargin, row.positionInitialMargin as i64, "@ {mark}");
            assert_eq!(probe(&mut ctx, G3), 0, "@ {mark}");
        }
    }

    /// T14: right after a real-fill open, at any `L`, nothing comes out (equity is one fee below
    /// `PIM`). It unlocks only once the move outruns the fee — `q·|m − e|·(1 ∓ 1/L) > fee` — and
    /// these are the exact last-locked / first-unlocked ticks on the fixture grid:
    /// `cap = |Δn|·(L ∓ 1)/L − fee` with `|Δn| = 3e4·|Δticks|` divisible by `L`. A ×1 long never
    /// unlocks (slope 0).
    #[test]
    fn t14_a_fresh_open_unlocks_only_once_the_move_outruns_the_fee() {
        for (long, lev, locked, unlocked, want) in [
            (true, 2u64, 10_020u64, 10_021u64, 15_000u64),
            (true, 3, 10_015, 10_016, 20_000),
            (false, 1, 9_995, 9_994, 60_000),
            (false, 2, 9_994, 9_993, 15_000),
            (false, 3, 9_993, 9_992, 20_000),
        ] {
            let (mut ctx, fee) = opened(long, Q as u64, lev, TAKER_BPS);
            assert_eq!(fee, OPEN_FEE);
            let row = risk_row(&mut ctx);
            assert_eq!(
                row.isolatedMargin - row.positionInitialMargin as i64,
                -(fee as i64),
                "{long}/{lev}: at the open, equity is one fee below PIM"
            );
            assert_eq!(probe(&mut ctx, G3), 0);
            for (mark, cap) in [(locked, 0u64), (unlocked, want)] {
                set_mark(&mut ctx, mark);
                let row = risk_row(&mut ctx);
                assert_regime(&row, Regime::Equity);
                let dn = (mark as i64 - ENTRY_PRICE as i64).abs() * Q * 10_000;
                let slope = if long { lev as i64 - 1 } else { lev as i64 + 1 };
                assert_eq!(dn % lev as i64, 0);
                assert_eq!(
                    cap as i64,
                    (dn * slope / lev as i64 - fee as i64).max(0),
                    "{long}/{lev} @ {mark}: closed form"
                );
                assert_eq!(probe(&mut ctx, G3), cap, "{long}/{lev} @ {mark}");
            }
        }
        let (mut ctx, _) = opened(true, Q as u64, 1, TAKER_BPS);
        for mark in [10_016u64, 15_000, 20_000] {
            set_mark(&mut ctx, mark);
            assert_regime(&risk_row(&mut ctx), Regime::Equity);
            assert_eq!(probe(&mut ctx, G3), 0, "a ×1 long never unlocks @ {mark}");
        }
    }

    // ── T6: the balance term [extrapolated] ──────────────────────────────────────────────────

    /// T6 (+ T9) [extrapolated]: the BALANCE term binds once `uPnL > PIM − MM`. Long `L = 3` @ 1.50
    /// and short `L = 3` @ 0.80: `cap + 1` is refused by G2 while its equity would still clear `PIM`
    /// — the balance alone refuses it. A fresh `L = 3` long with nothing added has cap 0 at 2.0;
    /// its cap peaks at 1.20, where the two terms are exactly equal (and G2, the first gate,
    /// names the refusal); just below it (1.19) G3 binds, just above it (1.21) G2.
    #[test]
    fn t6_the_balance_term_binds_past_pim_minus_mm() {
        for (amount, v_quote, mark, want) in [
            (Q, -N0, 15_000u64, 125_000_000u64),
            (-Q, N0, 8_000, 160_000_000),
        ] {
            let mut ctx = seeded(amount, v_quote, 200_000_000, 3, mark);
            let row = risk_row(&mut ctx);
            assert_regime(&row, Regime::Balance);
            assert_eq!(
                want as i64,
                200_000_000 - mark as i64 * Q * 10_000 / 6,
                "@ {mark}: iw − MM"
            );
            let over = want as i128 + 1;
            assert!(
                row.isolatedWallet as i128 - over + row.unrealizedProfit as i128
                    >= row.positionInitialMargin as i128,
                "@ {mark}: cap + 1 would still leave equity ≥ PIM — only the balance refuses it"
            );
            assert_eq!(probe(&mut ctx, G2), want, "@ {mark}");
            assert_not_liquidatable(&mut ctx);
        }

        let (mut ctx, fee) = opened(true, Q as u64, 3, TAKER_BPS);
        assert_eq!(fee, OPEN_FEE);
        set_mark(&mut ctx, 20_000);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::Balance);
        assert_eq!(
            row.isolatedWallet - row.maintMargin as i64,
            -(fee as i64),
            "+100%: iw − MM = −fee"
        );
        assert_eq!(probe(&mut ctx, G2), 0);

        // Either side of the 1.20 peak, each from its own fresh open: at 1.19 the equity term binds
        // (G3), at 1.21 the balance term (G2).
        for (mark, regime, gate, want) in [
            (11_900u64, Regime::Equity, G3, 37_700_000u64),
            (12_100, Regime::Balance, G2, 39_200_000),
        ] {
            let (mut ctx, _) = opened(true, Q as u64, 3, TAKER_BPS);
            set_mark(&mut ctx, mark);
            let row = risk_row(&mut ctx);
            assert_regime(&row, regime);
            assert_eq!(cap_of(&row), want, "@ {mark}: below the 1.20 peak");
            assert_eq!(probe(&mut ctx, gate), want, "@ {mark}");
        }
        let (mut ctx, _) = opened(true, Q as u64, 3, TAKER_BPS);
        set_mark(&mut ctx, 12_000);
        let row = risk_row(&mut ctx);
        let balance_term = row.isolatedWallet as i128 - row.maintMargin as i128;
        let equity_term = row.isolatedMargin as i128 - row.positionInitialMargin as i128;
        assert_eq!(balance_term, equity_term, "1.20·entry: the two terms meet");
        assert_eq!(regime_of(&row), None, "uPnL == PIM − MM exactly");
        assert_eq!(probe(&mut ctx, G2), 39_700_000);
        assert_not_liquidatable(&mut ctx);
    }

    // ── T7 / T8: whole refusal, and funding ──────────────────────────────────────────────────

    /// T7: a refusal is WHOLE — nothing clipped, nothing partially taken, and the pending funding
    /// NOT settled (index, margin, insurance fund, logs all untouched). With a 3e6 charge pending
    /// the settled silo is 196.7e6 (`PIM` 1e8, `MM` 5e7): 198e6 fits the UNsettled 199.7e6 but is
    /// refused by G1, 150e6 by G2, and 96.7e6 + 1 by G3. The full 96.7e6 is still there afterwards.
    /// The G3 refusal is also sent through the precompile dispatcher, whose debug-build
    /// write-then-revert tripwire fires on any perp write a reverted call leaves behind.
    #[test]
    fn t7_a_refusal_is_whole_and_settles_no_funding() {
        let mut ctx = seeded(Q, -N0, 199_700_000, 3, ENTRY_PRICE);
        assert_regime(&risk_row(&mut ctx), Regime::Equity);
        set_funding_index(&mut ctx, 100_000_000);
        for (amount, gate) in [
            (i64::MAX as u64, G1),
            (198_000_000u64, G1),
            (150_000_000, G2),
            (96_700_001, G3),
        ] {
            assert_refused_whole(&mut ctx, amount, gate);
            let pos = position(&mut ctx, ALICE);
            assert_eq!(
                (pos.margin, pos.last_funding_index),
                (199_700_000, 0),
                "{amount}: no settle"
            );
        }

        let _ = JournalTr::take_logs(ctx.journal_mut());
        let before = snapshot_on(&mut ctx, ALICE, MARKET_ID);
        let out = run_perp_dex_call(
            &removePositionMarginCall {
                marketId: MARKET_ID,
                amount: 96_700_001,
            }
            .abi_encode(),
            1_000_000,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(out.reverted, "an over-cap removePositionMargin must revert");
        assert_eq!(
            <alloy_sol_types::Revert as alloy_sol_types::SolError>::abi_decode(&out.bytes)
                .unwrap()
                .reason,
            G3
        );
        assert_eq!(
            snapshot_on(&mut ctx, ALICE, MARKET_ID),
            before,
            "a reverted call writes nothing and settles no funding"
        );
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());

        let wallet0 = wallet(&mut ctx, ALICE);
        remove_position_margin(&mut ctx, 96_700_000).unwrap();
        let pos = position(&mut ctx, ALICE);
        assert_eq!(pos.margin, 100_000_000, "196.7e6 settled − 96.7e6 = PIM");
        assert_eq!(pos.last_funding_index, 100_000_000);
        assert_eq!(wallet(&mut ctx, ALICE), wallet0 + 96_700_000);
    }

    /// T7 for G4: a safety-net refusal with funding pending settles nothing either. T15 route ①
    /// plus a 1e5 charge (−QTY·Δ/100): settled iw 599_900_000, settled G4 cap 83_233_334, so
    /// 83_233_335 clears G1–G3 after settlement (margin 516_666_665 ≥ MM; equity 466_666_665 ≥ PIM)
    /// and fails only G4. The unsettled rule would have accepted it (cap 83_333_334). Kills the
    /// mutant that settles funding before G4 instead of after every gate: the snapshot stays
    /// bit-identical there, so only the empty-logs check of `assert_refused_whole` sees it.
    #[test]
    fn t7_a_g4_refusal_with_funding_pending_settles_nothing() {
        let mut ctx = seeded(QTY, -1_050_000_000, 600_000_000, 3, ENTRY_PRICE);
        set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 100_000_000], &[3, 1]).unwrap();
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::SafetyNet);
        assert_eq!(cap_of(&row), 83_333_334);
        set_funding_index(&mut ctx, 1_000_000);
        assert_refused_whole(&mut ctx, 83_233_335, G4);
        assert_eq!(position(&mut ctx, ALICE).last_funding_index, 0);
        remove_position_margin(&mut ctx, 83_233_334).unwrap();
        let pos = position(&mut ctx, ALICE);
        assert_eq!((pos.margin, pos.last_funding_index), (516_666_666, 1_000_000));
        assert_eq!(cap_of(&risk_row(&mut ctx)), 0);
    }

    /// Shared T8 body: `L = 3` long at the entry mark, silo 199.7e6, `PIM` 1e8, `MM` 5e7 — so the
    /// cap BEFORE settlement is 99.7e6 — then a funding index that moves the silo by `funding`.
    /// `between` lies between the pre- and post-settlement caps, so the two algorithms disagree on
    /// it; the engine must follow the settled one. Returns the post-settlement cap.
    fn t8_case(index: i128, funding: i64, between: u64) -> (TestCtx, u64) {
        let mut ctx = seeded(Q, -N0, 199_700_000, 3, ENTRY_PRICE);
        let pre = risk_row(&mut ctx);
        assert_regime(&pre, Regime::Equity);
        let cap_pre = cap_of(&pre);
        assert_eq!(cap_pre, 99_700_000);
        set_funding_index(&mut ctx, index);
        assert_eq!(funding, -(Q as i128 * index / 100) as i64, "−q·Δ/100 on this grid");
        // Both terms move one-for-one with `iw` and neither clamps, so the settled cap is the
        // unsettled one plus the funding.
        let cap_post = (cap_pre as i64 + funding) as u64;
        assert!(
            (cap_pre.min(cap_post)..=cap_pre.max(cap_post)).contains(&between)
                && between != cap_pre
                && between != cap_post
        );
        assert_refused_whole(&mut ctx, cap_post + 1, G3);
        (ctx, cap_post)
    }

    /// The success stream of a removal that settles funding first: the funding group, then the
    /// margin-transfer group — `FundingSettled → … → PositionMarginAdjusted → … PositionChanged`.
    fn remove_and_pin_stream(ctx: &mut TestCtx, amount: u64, funding: i64) {
        let pos0 = position(ctx, ALICE);
        let wallet0 = storage::load_account(ctx, ALICE).unwrap().perp_wallet_balance;
        let _ = JournalTr::take_logs(ctx.journal_mut());
        storage::begin_perp_call(ctx);
        remove_position_margin(ctx, amount).unwrap();
        storage::flush_account_snapshots(ctx).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_eq!(
            stream_shape(&logs),
            vec![
                ("FundingSettled", Some(ALICE)),
                ("AccountBalanceChanged", Some(ALICE)),
                ("PositionChanged", Some(ALICE)),
                ("PositionMarginAdjusted", Some(ALICE)),
                ("AccountBalanceChanged", Some(ALICE)),
                ("PositionChanged", Some(ALICE)),
            ]
        );
        assert_eq!(
            account_update_reasons(&logs),
            vec![(ALICE, R::FundingFee), (ALICE, R::MarginTransfer)]
        );
        let settled = decode::<FundingSettled>(&logs);
        assert_eq!(settled[0].amount, funding);
        let pos1 = position(ctx, ALICE);
        let adjusted = decode::<PositionMarginAdjusted>(&logs);
        assert_eq!((adjusted[0].delta, adjusted[0].margin), (-(amount as i64), pos1.margin));
        assert_eq!(pos1.margin, pos0.margin + funding - amount as i64);
        assert_eq!(
            storage::load_account(ctx, ALICE).unwrap().perp_wallet_balance,
            wallet0 + amount as i64,
            "funding lands in the silo, never the wallet"
        );
    }

    /// T8, PAY: a 3e6 charge pending. 98e6 is inside the unsettled cap (99.7e6) and outside the
    /// settled one (96.7e6) — refused by G3, nothing settled. The settled cap comes out exactly.
    #[test]
    fn t8_pending_funding_charge_is_settled_before_the_cap_is_judged() {
        let (mut ctx, cap_post) = t8_case(100_000_000, -3_000_000, 98_000_000);
        assert_eq!(cap_post, 96_700_000);
        assert_refused_whole(&mut ctx, 98_000_000, G3);
        assert_eq!(position(&mut ctx, ALICE).last_funding_index, 0);
        remove_and_pin_stream(&mut ctx, cap_post, -3_000_000);
        let pos = position(&mut ctx, ALICE);
        assert_eq!((pos.margin, pos.last_funding_index), (100_000_000, 100_000_000));
        assert_eq!(cap_of(&risk_row(&mut ctx)), 0);
    }

    /// T8, RECEIVE: a 3e6 credit pending. 101e6 is outside the unsettled cap (99.7e6) and inside the
    /// settled one (102.7e6) — honoured. What is left (1.7e6) is then exactly the next cap.
    #[test]
    fn t8_pending_funding_credit_is_settled_before_the_cap_is_judged() {
        let (mut ctx, cap_post) = t8_case(-100_000_000, 3_000_000, 101_000_000);
        assert_eq!(cap_post, 102_700_000);
        remove_and_pin_stream(&mut ctx, 101_000_000, 3_000_000);
        let pos = position(&mut ctx, ALICE);
        assert_eq!((pos.margin, pos.last_funding_index), (101_700_000, -100_000_000));
        assert_eq!(probe(&mut ctx, G3), 1_700_000);
    }

    // ── T10 / T11 / T12 ──────────────────────────────────────────────────────────────────────

    /// T10: the precondition errors are unchanged, each refusing whole. The wallet overflow fires
    /// only AFTER all four gates pass. (It cannot be combined with pending funding: the funding
    /// settle's own `AccountBalanceChanged` payload overflows first on a wallet at `i64::MAX`.)
    #[test]
    fn t10_precondition_errors_are_unchanged() {
        let mut ctx = seeded(Q, -N0, 199_700_000, 3, ENTRY_PRICE);
        assert_refused_whole(&mut ctx, 0, "removePositionMargin: amount must be > 0");
        for amount in [i64::MAX as u64 + 1, u64::MAX] {
            assert_refused_whole(
                &mut ctx,
                amount,
                "removePositionMargin: amount: value exceeds i64::MAX",
            );
        }
        assert_refused_whole_on(
            &mut ctx,
            ALICE,
            MARKET_ID + 7,
            1,
            "removePositionMargin: unknown market",
        );
        assert_refused_whole_on(
            &mut ctx,
            KEEPER,
            MARKET_ID,
            1,
            "removePositionMargin: no open position",
        );
        set_mark(&mut ctx, 0);
        assert_refused_whole(&mut ctx, 1, "removePositionMargin: mark price unavailable");

        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, Q, -N0, 199_700_000, 3, i64::MAX);
        let row = risk_row(&mut ctx);
        assert_eq!((cap_of(&row), gate_for(&row, 1)), (99_700_000, None), "1 clears every gate");
        assert_refused_whole(&mut ctx, 1, "perp wallet: balance overflow");
    }

    /// T11: a resting BUY far below the mark (`ooIM` 5e7 > 0) does not move the cap — only the
    /// POSITION term `PIM` enters it, never `IM` or `ooIM`. Taking the cap raises
    /// `availableBalance` by exactly the amount and leaves `openOrderInitialMargin` unchanged.
    #[test]
    fn t11_a_resting_order_does_not_move_the_cap() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, Q, -N0, 200_000_000, 3, 1_000_000_000);
        set_mark(&mut ctx, 11_000);
        let before = risk_row(&mut ctx);
        assert_regime(&before, Regime::Equity);
        assert_eq!(before.openOrderInitialMargin, 0);
        let cap_before = cap_of(&before);
        assert_eq!(cap_before, 120_000_000);

        place_order(&mut ctx, ALICE, Side::Buy as u8, 5_000, Q as u64);
        let row = risk_row(&mut ctx);
        assert_eq!(
            row.openOrderInitialMargin, 50_000_000,
            "⌈(3.3e8 + 1.5e8)/3⌉ − 1.1e8"
        );
        assert_eq!(
            (
                row.isolatedWallet,
                row.unrealizedProfit,
                row.positionInitialMargin,
                row.maintMargin
            ),
            (
                before.isolatedWallet,
                before.unrealizedProfit,
                before.positionInitialMargin,
                before.maintMargin
            )
        );
        assert_eq!(cap_of(&row), cap_before, "bit-identical cap with the order resting");

        let available0 = crate::margin_view::derived_available_balance(&mut ctx, ALICE).unwrap();
        assert_eq!(probe(&mut ctx, G3), cap_before);
        let available1 = crate::margin_view::derived_available_balance(&mut ctx, ALICE).unwrap();
        assert_eq!(available1 - available0, cap_before as i128);
        assert_eq!(risk_row(&mut ctx).openOrderInitialMargin, row.openOrderInitialMargin);
    }

    /// T12: raising leverage lowers `PIM` and raises the cap to the new one. Opened at `L = 1`
    /// (real fill, 10 bps): cap 0. At `L = 2`: `PIM` 1.5e8, cap 149.7e6. At `L = 3`: `PIM` 1e8,
    /// cap 199.7e6. `setLeverage` moves no margin.
    #[test]
    fn t12_raising_leverage_raises_the_cap_to_the_new_pim() {
        let (mut ctx, fee) = opened(true, Q as u64, 1, TAKER_BPS);
        assert_eq!(fee, OPEN_FEE);
        let row = risk_row(&mut ctx);
        assert_eq!(row.positionInitialMargin, 300_000_000);
        assert_eq!(cap_of(&row), 0);
        assert_refused_whole(&mut ctx, 1, G3);
        let margin = position(&mut ctx, ALICE).margin;

        set_leverage(&mut ctx, 2).unwrap();
        assert_eq!(position(&mut ctx, ALICE).margin, margin);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::Equity);
        assert_eq!(row.positionInitialMargin, 150_000_000);
        assert_eq!(cap_of(&row), 149_700_000);
        assert_refused_whole(&mut ctx, 149_700_001, G3);
        // The accept side at L = 2, on a fresh open — taking the cap here would move the silo the
        // L = 3 step below reads.
        let (mut fresh, _) = opened(true, Q as u64, 1, TAKER_BPS);
        set_leverage(&mut fresh, 2).unwrap();
        assert_eq!(probe(&mut fresh, G3), 149_700_000);

        set_leverage(&mut ctx, 3).unwrap();
        assert_eq!(position(&mut ctx, ALICE).margin, margin);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::Equity);
        assert_eq!(row.positionInitialMargin, 100_000_000);
        assert_eq!(probe(&mut ctx, G3), 199_700_000);
    }

    // ── T13: the property test ───────────────────────────────────────────────────────────────

    /// Deterministic xorshift (same generator as the settlement conservation fuzz).
    fn next(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    fn tier(lower_bound_notional: u64, max_leverage: u32) -> MarginTier {
        MarginTier {
            lower_bound_notional,
            max_leverage,
        }
    }

    /// T13: on random states — both signs, `L` 1..=3, random size / entry / margin / mark, random
    /// tier tables (including ones with `MM > PIM`), on the fixture grid AND on a fractional
    /// `price_decimals = 8` grid where the notional truncates and `PIM` rounds up — the gate's
    /// accept boundary is EXACTLY the cap computed from `getPositionRisk`: `cap + 1` is refused
    /// whole by the gate the row names, and `cap` is honoured to the unit. About a third of the
    /// states also rest orders, so `IM ≠ PIM` there and a G3 reading `initialMargin` fails. Coverage
    /// is asserted, not hoped for.
    #[test]
    fn t13_the_accept_boundary_is_the_get_position_risk_cap_on_random_states() {
        let mut seed: u64 = 0x2b5e_ed00_c0ff_ee11;
        // The resting orders draw from their OWN stream, so the positions, marks and tables are the
        // same 3_000 states with or without them.
        let mut order_seed: u64 = 0x0dd5_1de5_b1d5_a5c3;
        let mut gates: std::collections::BTreeMap<&str, usize> = Default::default();
        let mut regimes: std::collections::BTreeMap<Option<Regime>, usize> = Default::default();
        let (mut clamped, mut taken, mut frac_truncated, mut pim_rounded) = (0, 0, 0, 0);
        let mut with_ooim = 0;
        for _ in 0..3_000 {
            let frac = next(&mut seed) % 2 == 1;
            let (market_id, price_decimals) = if frac {
                (FRAC_MARKET, FRAC_PRICE_DECIMALS)
            } else {
                (MARKET_ID, PRICE_DECIMALS)
            };
            let entry_cents = 1_000 + next(&mut seed) % 99_000;
            let entry = if frac {
                entry_cents * 1_000_000 + next(&mut seed) % 1_000_000
            } else {
                entry_cents
            };
            let qty = 1 + next(&mut seed) % 500;
            let long = next(&mut seed) % 2 == 0;
            let n_entry = calc_value(entry, qty, 0, price_decimals).unwrap() as i64;
            let jitter = (next(&mut seed) % 2_001) as i64 - 1_000;
            let v_quote_abs = (n_entry + jitter).max(1);
            let lev = 1 + next(&mut seed) % 3;
            let mark_pct = 50 + next(&mut seed) % 101;
            let mark = ((entry as u128 * mark_pct as u128 / 100) as u64 + next(&mut seed) % 7).max(1);
            // Up to `2N` half the time: a `MM > PIM` table needs equity above `N/2` before G4 has
            // anything left to bind on.
            let margin = match next(&mut seed) % 16 {
                0 => 0,
                1 => (next(&mut seed) % 1_000) as i64,
                2..=8 => (next(&mut seed) % (2 * n_entry as u64 / lev + 1)) as i64,
                _ => (next(&mut seed) % (2 * n_entry as u64 + 1)) as i64,
            };
            let x = 1 + next(&mut seed) % (2 * n_entry as u64);
            let rows = match next(&mut seed) % 6 {
                0 | 1 => vec![tier(0, 3)],
                2 => vec![tier(0, 3), tier(x, 1)],
                3 => vec![tier(0, 1)],
                4 => vec![tier(0, 2)],
                _ => vec![tier(0, 3), tier(x, 2), tier(2 * x, 1)],
            };
            let wallet = (next(&mut seed) % 1_000_000_000_000) as i64;

            // One resting buy and/or one resting sell, each worth up to ~3× the entry notional:
            // `Bid`/`Ask` move `IM` off `PIM` and must not move the cap. They are installed as real
            // order lists with the side aggregates folded from them, never as bare aggregates:
            // `compute_margin_info`'s debug-only oracle compares the stored `Bid`/`Ask` against
            // that fold on every `getPositionRisk` read AND on the account fold behind every
            // accepted withdrawal's `AccountBalanceChanged`, so aggregates with no orders behind
            // them would trip it.
            let mut sides: [std::collections::VecDeque<crate::types::OrderEntry>; 2] =
                Default::default();
            if next(&mut order_seed) % 3 == 0 {
                for (i, list) in sides.iter_mut().enumerate() {
                    if next(&mut order_seed) % 4 == 0 {
                        continue;
                    }
                    let price = ((entry as u128 * (50 + next(&mut order_seed) % 101) as u128)
                        / 100)
                        .max(1) as u64;
                    let mut order_id = [0u8; 32];
                    order_id[31] = 1 + i as u8;
                    list.push_back(crate::types::OrderEntry {
                        order_id,
                        price,
                        amount: 1 + next(&mut order_seed) % (2 * qty),
                        maker_fee_bps: 0,
                        assuming_price: price,
                        reduce_only: false,
                    });
                }
            }
            let [buys, sells] = sides;
            let fold = |list: &std::collections::VecDeque<crate::types::OrderEntry>| {
                crate::math::sum_side_totals(list.iter().copied(), 0, price_decimals).unwrap()
            };
            let ((buy_qty, bid), (sell_qty, ask)) = (fold(&buys), fold(&sells));

            let mut ctx = make_ctx();
            storage::save_admin(&mut ctx, ADMIN).unwrap();
            storage::save_market(
                &mut ctx,
                &Market {
                    market_id,
                    base_decimals: 0,
                    price_decimals,
                    tick_size: 1,
                    step_size: 1,
                    min_quantity: 1,
                    max_quantity: 1_000_000,
                    max_price: u64::MAX / 4,
                    price_update_interval: 15,
                    active: true,
                    funding_interval: 0,
                    interest_rate: 0,
                    liquidation_fee_rate_bps: 0,
                    price_band_bps: 0,
                    mark_price: mark,
                    tiers: MarginTiers::from_tiers(&rows).unwrap(),
                    basis_mode: 0,
                    deferred_work: 0,
                },
            )
            .unwrap();
            storage::save_account(
                &mut ctx,
                ALICE,
                UserAccount {
                    perp_wallet_balance: wallet,
                    ..UserAccount::default()
                },
                AccountUpdateReason::Adjustment,
            )
            .unwrap();
            storage::save_position(
                &mut ctx,
                ALICE,
                market_id,
                &PerpPosition {
                    amount: if long { qty as i64 } else { -(qty as i64) },
                    v_quote_balance: if long { -v_quote_abs } else { v_quote_abs },
                    margin,
                    leverage: lev,
                    total_buy_qty: buy_qty,
                    total_buy_notional: bid,
                    total_sell_qty: sell_qty,
                    total_sell_notional: ask,
                    ..PerpPosition::default()
                },
                AccountUpdateReason::Adjustment,
            )
            .unwrap();
            if !buys.is_empty() {
                storage::save_buy_orders(&mut ctx, ALICE, market_id, &buys).unwrap();
            }
            if !sells.is_empty() {
                storage::save_sell_orders(&mut ctx, ALICE, market_id, &sells).unwrap();
            }

            let row = risk_row_on(&mut ctx, ALICE, market_id);
            assert_eq!((row.bidNotional, row.askNotional), (bid, ask));
            if row.openOrderInitialMargin > 0 {
                with_ooim += 1;
            }
            let cap = cap_of(&row);
            let gate = gate_for(&row, cap + 1).expect("cap + 1 must fail some gate");
            *gates.entry(gate).or_default() += 1;
            *regimes.entry(regime_of(&row)).or_default() += 1;
            if frac && (mark as u128 * qty as u128) % 100 != 0 {
                frac_truncated += 1;
            }
            if row.notional % lev != 0 {
                pim_rounded += 1;
            }
            assert_eq!(probe_on(&mut ctx, ALICE, market_id, gate), cap);
            if cap == 0 {
                clamped += 1;
            } else {
                taken += 1;
            }
        }
        for g in [G1, G2, G3, G4] {
            assert!(gates.get(g).copied().unwrap_or(0) >= 20, "gate `{g}` under-covered: {gates:?}");
        }
        for r in [Regime::Equity, Regime::Balance, Regime::SafetyNet] {
            assert!(regimes.get(&Some(r)).copied().unwrap_or(0) >= 20, "{r:?}: {regimes:?}");
        }
        assert!(clamped >= 100 && taken >= 1_000, "clamped {clamped}, taken {taken}");
        assert!(frac_truncated >= 500, "truncating notionals: {frac_truncated}");
        assert!(pim_rounded >= 500, "rounded-up PIMs: {pim_rounded}");
        assert!(with_ooim >= 200, "states with openOrderInitialMargin > 0: {with_ooim}");
    }

    // ── T15: the G4 safety net ───────────────────────────────────────────────────────────────

    /// T15 (+ T9), route ①: `setMarginTiers` installs a LOWER-leverage table under a live losing
    /// `L = 3` long — nothing re-checks open positions. Long 10, `vQuote` −1.05e9 (entry $105),
    /// margin 600M, mark $100, tiers `[0, 100M] / [3×, 1×]`: `MM = 100M/6 + 900M/2 = 466_666_666`
    /// against `PIM = 333_333_334`, `uPnL = −50M`. Terms G2 133_333_334, G3 216_666_666, G4
    /// 83_333_334 — G4 binds, and `cap + 1` clears G2 and G3 and fails G4 with its FULL string.
    /// Equity then sits exactly on `MM`, and `liquidate()` is refused.
    #[test]
    fn t15_g4_binds_after_set_margin_tiers_lowers_leverage_under_a_live_position() {
        let mut ctx = seeded(QTY, -1_050_000_000, 600_000_000, 3, ENTRY_PRICE);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::Equity);
        assert_eq!(cap_of(&row), 216_666_666, "on the default table G3 binds");

        set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 100_000_000], &[3, 1]).unwrap();
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::SafetyNet);
        assert_eq!(row.maintMargin, 466_666_666);
        assert_eq!(row.positionInitialMargin, 333_333_334);
        assert_eq!(row.unrealizedProfit, -50_000_000);
        assert_eq!(probe(&mut ctx, G4), 83_333_334);
        let after = risk_row(&mut ctx);
        assert_eq!(after.isolatedMargin, after.maintMargin as i64, "equity == MM exactly");
        assert_not_liquidatable(&mut ctx);
    }

    /// T15 (+ T9), route ②: a SHORT whose notional drifts into a 1× tier as the mark rises. Tiers
    /// `[0, 150M] / [3×, 1×]` are in place BEFORE a real-fill `L = 3` open of 1 @ $100 (`N = 1e8`,
    /// tier 0, so the per-open tier guard passes), then `addPositionMargin(500M)`. At $350
    /// `|N| = 3.5e8 > 2·150M`: `MM = 150M/6 + 200M/2 = 125M > PIM = ⌈3.5e8/3⌉ = 116_666_667`,
    /// `uPnL = −250M`. The cap `iw + uPnL − MM` is G4's.
    #[test]
    fn t15_g4_binds_when_a_short_drifts_into_a_lower_leverage_tier() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 150_000_000], &[3, 1]).unwrap();
        let fee = open_via_fill(&mut ctx, false, 1, 3, TAKER_BPS);
        assert_eq!(fee, 100_000);
        add_position_margin(&mut ctx, 500_000_000).unwrap();
        let iw = 100_000_000 / 3 - fee as i64 + 500_000_000;
        assert_eq!(position(&mut ctx, ALICE).margin, iw);
        set_mark(&mut ctx, 35_000);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::SafetyNet);
        assert_eq!(row.maintMargin, 125_000_000);
        assert_eq!(row.positionInitialMargin, 116_666_667);
        assert_eq!(row.unrealizedProfit, -250_000_000);
        let want = (iw - 250_000_000 - 125_000_000) as u64;
        assert_eq!(want, 158_233_333);
        assert_eq!(probe(&mut ctx, G4), want);
        assert_not_liquidatable(&mut ctx);
    }

    // ── T16: life after the cap ──────────────────────────────────────────────────────────────

    fn total_wallet(ctx: &mut TestCtx) -> i64 {
        storage::load_account(ctx, ALICE).unwrap().perp_wallet_balance + position(ctx, ALICE).margin
    }

    /// Every `PositionChanged` row in `logs` has a NON-NEGATIVE margin. **Only valid for a scenario
    /// with no funding DEBT.** A funding charge lands on `pos.margin` whole and may drive it below
    /// zero (`crate::funding`), and the rows then report that debt — correctly. The T16 scenarios
    /// that use this charge at most 60_000 against a 160e6 silo, so here a negative row can only
    /// mean a broken close release; a scenario that charges more funding than the silo holds must
    /// not use this helper (see `mod funding_debt` for those).
    fn assert_margins_nonnegative_absent_funding_debt(logs: &[primitives::Log]) {
        for row in decode::<PositionChanged>(logs) {
            assert!(row.margin >= 0, "PositionChanged({}) margin {}", row.user, row.margin);
        }
    }

    /// `getPositionRisk.liquidationPrice` is the flip point of the production predicate: for a
    /// long liquidatable AT it and not one tick above; for a short at it and not one tick below.
    fn assert_liquidation_price_is_the_predicates_flip(ctx: &mut TestCtx) -> u64 {
        let lp = risk_row(ctx).liquidationPrice;
        let pos = position(ctx, ALICE);
        let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
        let above = |p: u64| {
            is_above_maintenance_margin(
                &market.tiers,
                p,
                pos.amount,
                pos.v_quote_balance,
                pos.margin,
                0,
                PRICE_DECIMALS,
            )
            .unwrap()
        };
        assert!(lp > 0);
        assert!(!above(lp), "liquidatable at the reported price {lp}");
        if pos.amount > 0 {
            assert!(above(lp + 1), "and not one tick above it");
        } else {
            assert!(above(lp - 1), "and not one tick below it");
        }
        lp
    }

    /// Long 6 @ $100 at `L = 3` through a real fill (10 bps: fee 600_000, silo 199.4e6), marked to
    /// $110 (`PIM` 2.2e8, `MM` 1.1e8, `uPnL` 6e7), and the cap 39.4e6 taken: the silo is 160e6 —
    /// BELOW `PIM`. Its liquidation price is $87.99: `6e4·P − 4.4e8 ≥ 1e4·P ⇔ P ≥ 8_800`.
    fn withdrawn_to_cap() -> TestCtx {
        let (mut ctx, fee) = opened(true, 6, 3, TAKER_BPS);
        assert_eq!(fee, 600_000);
        set_mark(&mut ctx, 11_000);
        assert_regime(&risk_row(&mut ctx), Regime::Equity);
        assert_eq!(probe(&mut ctx, G3), 39_400_000);
        let row = risk_row(&mut ctx);
        assert_eq!(row.isolatedWallet, 160_000_000);
        assert!(row.isolatedWallet < row.positionInitialMargin as i64);
        assert_eq!(assert_liquidation_price_is_the_predicates_flip(&mut ctx), 8_799);
        ctx
    }

    /// ALICE sells `qty` at the $110 mark into a MAKER bid. Returns `(takerFee, takerRealizedPnl,
    /// ALICE's settled funding, logs)`.
    fn alice_sells_at_110(ctx: &mut TestCtx, qty: u64) -> (u64, i64, i64, Vec<primitives::Log>) {
        place_maker_order(ctx, Side::Buy as u8, 11_000, qty);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        try_place_order(ctx, ALICE, Side::Sell as u8, 11_000, qty).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());
        let trades = decode::<Trade>(&logs);
        assert_eq!(trades.len(), 1);
        assert_eq!((trades[0].taker, trades[0].quantity), (ALICE, qty));
        let funding = decode::<FundingSettled>(&logs)
            .into_iter()
            .filter(|f| f.user == ALICE)
            .map(|f| f.amount)
            .sum();
        assert_margins_nonnegative_absent_funding_debt(&logs);
        (trades[0].takerFee, trades[0].takerRealizedPnl, funding, logs)
    }

    /// T16 ①: a 50% close after the cap, with a funding charge pending (settled by the fill):
    /// total wallet moves by `realized − fee + funding` = 3e7 − 330_000 − 60_000; the half left
    /// keeps a non-negative silo and an exact liquidation price.
    #[test]
    fn t16_partial_close_after_the_cap() {
        let mut ctx = withdrawn_to_cap();
        set_funding_index(&mut ctx, 1_000_000);
        let tw0 = total_wallet(&mut ctx);
        let (fee, realized, funding, _) = alice_sells_at_110(&mut ctx, 3);
        assert_eq!((fee, realized, funding), (330_000, 30_000_000, -60_000));
        assert_eq!(total_wallet(&mut ctx) - tw0, realized - fee as i64 + funding);
        let pos = position(&mut ctx, ALICE);
        assert_eq!(pos.amount, 3);
        assert_eq!(pos.margin, (160_000_000 - 60_000) / 2, "half the settled silo stays");
        assert_liquidation_price_is_the_predicates_flip(&mut ctx);
    }

    /// T16 ②: a flip after the cap (sell 12): the closing leg realizes 6e7, the opening short is
    /// funded `⌊6.6e8/3⌋` from the wallet with the whole 1.32e6 fee out of it; total wallet moves by
    /// `realized − fee`.
    #[test]
    fn t16_flip_after_the_cap() {
        let mut ctx = withdrawn_to_cap();
        let tw0 = total_wallet(&mut ctx);
        let (fee, realized, funding, _) = alice_sells_at_110(&mut ctx, 12);
        assert_eq!((fee, realized, funding), (1_320_000, 60_000_000, 0));
        assert_eq!(total_wallet(&mut ctx) - tw0, realized - fee as i64);
        let pos = position(&mut ctx, ALICE);
        assert_eq!(
            (pos.amount, pos.v_quote_balance, pos.margin),
            (-6, 660_000_000, 220_000_000 - 1_320_000)
        );
        assert_liquidation_price_is_the_predicates_flip(&mut ctx);
    }

    /// T16 ③: a full close after the cap: realized 6e7, fee 660_000, silo released to 0.
    #[test]
    fn t16_full_close_after_the_cap() {
        let mut ctx = withdrawn_to_cap();
        let tw0 = total_wallet(&mut ctx);
        let (fee, realized, funding, _) = alice_sells_at_110(&mut ctx, 6);
        assert_eq!((fee, realized, funding), (660_000, 60_000_000, 0));
        assert_eq!(total_wallet(&mut ctx) - tw0, realized - fee as i64);
        let pos = position(&mut ctx, ALICE);
        assert_eq!((pos.amount, pos.margin), (0, 0));
        assert_eq!(risk_row(&mut ctx).liquidationPrice, 0, "flat: no liquidation price");
    }

    /// T16 ④: liquidation after the cap. The reported $87.99 is honoured by `liquidate()` on both
    /// sides — refused at $88.00, executed at $87.99 into a MAKER bid. Taker fee waived; a 5%
    /// clearance fee on the 160e6 silo closed (8e6) goes to the insurance fund. Realized
    /// `6·87.99·1e6 − 6e8 = −72_060_000`; the silo covers it (no bad debt); total wallet moves by
    /// `realized − clearance fee`.
    #[test]
    fn t16_liquidation_after_the_cap() {
        let mut ctx = withdrawn_to_cap();
        let mut market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
        market.liquidation_fee_rate_bps = 500;
        storage::save_market(&mut ctx, &market).unwrap();

        set_mark(&mut ctx, 8_800);
        assert_not_liquidatable(&mut ctx);
        set_mark(&mut ctx, 8_799);
        assert_eq!(assert_liquidation_price_is_the_predicates_flip(&mut ctx), 8_799);
        place_maker_order(&mut ctx, Side::Buy as u8, 8_799, 6);
        let tw0 = total_wallet(&mut ctx);
        let if0 = storage::load_insurance_fund(&mut ctx).unwrap();
        let _ = JournalTr::take_logs(ctx.journal_mut());
        liquidate(&mut ctx, ALICE).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_margins_nonnegative_absent_funding_debt(&logs);

        let trades = decode::<Trade>(&logs);
        assert_eq!(trades.len(), 1);
        assert_eq!(
            (trades[0].taker, trades[0].takerFee, trades[0].takerRealizedPnl),
            (ALICE, 0, -72_060_000)
        );
        let liquidation = decode::<Liquidation>(&logs);
        assert_eq!(liquidation.len(), 1);
        let clearance = liquidation[0].reward;
        assert_eq!(clearance, 8_000_000);
        assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap() - if0, clearance);
        assert_eq!(
            total_wallet(&mut ctx) - tw0,
            trades[0].takerRealizedPnl - clearance as i64
        );
        let pos = position(&mut ctx, ALICE);
        assert_eq!((pos.amount, pos.margin), (0, 0));
    }
}

// ── Funding out of EQUITY: the debt a funding charge can leave on a position ─────────────────────
//
// A funding charge lands on `pos.margin` WHOLE (`crate::funding`), so a position that keeps paying
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
mod funding_debt {
    use super::*;
    use crate::events::stream_test_support::stream_shape;
    use crate::interface::IPerpDex::{
        Adl, FundingSettled, InsuranceFundChanged, InsuranceFundDepleted, Liquidation,
        OrderCancelled, PositionChanged, PositionMarginAdjusted, Trade,
    };
    use crate::math::{calc_bankruptcy_price, calc_funding_payment, maintenance_margin};

    const SELL: u8 = Side::Sell as u8;
    const BUY: u8 = Side::Buy as u8;
    /// $130.00 — the mark at which a $100 long is held up by $300 of unrealized profit.
    const MARK_130: u64 = 13_000;
    /// Notional of `QTY` at `MARK_130`.
    const N_130: i64 = 1_300_000_000;
    /// `⌊N_130 / 6⌋`.
    const MM_130: i64 = 216_666_666;
    /// `⌊ENTRY_VALUE / 6⌋`.
    const MM_100: i64 = 166_666_666;
    /// Funding index giving a 250M charge on the `QTY` long (`2.5e9 / 10`).
    const INDEX_250M: i128 = 2_500_000_000;

    /// ALICE's funding settled AND PERSISTED — exactly the write `liquidate_position` makes
    /// (`compute` → `apply` → `save_position(FundingFee)`) — so a test can start from a STORED
    /// funding debt and measure the next operation on its own. Its funding events are discarded.
    fn persist_alice_funding(ctx: &mut TestCtx) -> PerpPosition {
        let (pos, _) = settle_alice_funding(ctx);
        storage::save_position(ctx, ALICE, MARKET_ID, &pos, AccountUpdateReason::FundingFee)
            .unwrap();
        let _ = JournalTr::take_logs(ctx.journal_mut());
        pos
    }

    /// `seed_position_account` with an explicit funding anchor, for counterparties that must (or
    /// must not) carry pending funding at the current index.
    fn seed_anchored(
        ctx: &mut TestCtx,
        user: Address,
        amount: i64,
        v_quote: i64,
        margin: i64,
        wallet: i64,
        last_funding_index: i128,
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
                leverage: 5,
                last_funding_index,
                ..PerpPosition::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
    }

    fn set_taker_bps(ctx: &mut TestCtx, user: Address, bps: u64) {
        storage::save_user_fee_rates(
            ctx,
            user,
            UserFeeRates {
                maker_fee_bps: 0,
                taker_fee_bps: bps,
            },
        )
        .unwrap();
    }

    fn set_wallet(ctx: &mut TestCtx, user: Address, wallet: i64) {
        let mut a = storage::load_account(ctx, user).unwrap();
        a.perp_wallet_balance = wallet;
        storage::save_account(ctx, user, a, AccountUpdateReason::Adjustment).unwrap();
    }

    fn signed_wallet(ctx: &mut TestCtx, user: Address) -> i64 {
        storage::load_account(ctx, user).unwrap().perp_wallet_balance
    }

    /// The stored `Σ pos.margin` scalar agrees with the one position the user holds — the Σ the
    /// `AccountBalanceChanged` header and `totalWalletBalance` are built from. Returns it.
    fn assert_sigma_margin_consistent(ctx: &mut TestCtx, user: Address) -> i64 {
        let sigma = storage::load_account(ctx, user).unwrap().total_position_margin;
        assert_eq!(sigma, position(ctx, user).margin, "{user}: stored Σ pos.margin drifted");
        sigma
    }

    fn sell_as_alice(ctx: &mut TestCtx, price: u64, qty: u64) -> Vec<primitives::Log> {
        let _ = JournalTr::take_logs(ctx.journal_mut());
        try_place_order(ctx, ALICE, SELL, price, qty).unwrap();
        JournalTr::take_logs(ctx.journal_mut())
    }

    // ── C1: the sweep finds a profit-supported long at exactly the closed-form epoch ─────────────

    /// **A long held up by unrealized profit is liquidated by funding alone, at exactly the epoch the
    /// closed form predicts.**
    ///
    /// Long 10 @ $100 with a silo `M = 199_416_666`, marked at $130: `U = 300M ≥ MM = 216_666_666`.
    /// Under the clamp this replaced, equity was `max(M − F, 0) + U ≥ U > MM` for EVERY cumulative
    /// charge `F` — the position could never be liquidated at this mark, and once the silo hit 0 the
    /// insurance fund paid its funding every epoch. Now equity is `M + U − F`, so it is liquidatable
    /// iff `F > M + U − MM = 282_750_000`.
    ///
    /// Each epoch charges the CAPPED rate, `mark × MAX_FUNDING_RATE = 13_000 × 7_500 = 97_500_000`
    /// of index, i.e. 9_750_000 (0.75% of the notional) on this long, so
    /// `k* = ⌊282_750_000 / 9_750_000⌋ + 1 = 30`. `M` is chosen so the threshold is EXACTLY 29
    /// epochs' charge: at epoch 29 (`F = 282_750_000`) equity is `216_666_666 == MM` — healthy only
    /// because the predicate is `>=` — and at epoch 30 (`F = 292_500_000`) it is
    /// `206_916_666 < MM`. So the test pins the boundary itself, not just the epoch: a strict `>`
    /// would liquidate one epoch early. At `k* − 1` a manual `liquidate` is also refused ("above
    /// maintenance margin") — a positive witness that the predicate ran and said healthy, not
    /// merely that nothing happened.
    ///
    /// Driven through REAL oracle ticks at an unchanged $130 index: the sweep is what must find
    /// it, with nobody touching the position. Every tick before `k*` must leave the position
    /// exactly as stored (a healthy candidate is write-free, so funding is never persisted and
    /// each tick prices the WHOLE accrued delta with a single floor). The liquidation at `k*` is
    /// SOLVENT (equity +206_916_666), so the empty-book residual ADLs at MARK against KEEPER — a
    /// short that has been RECEIVING the same funding, unsettled, and is paid it at the fill (C7).
    #[test]
    fn c1_a_profit_supported_long_paying_funding_is_swept_at_exactly_the_closed_form_epoch() {
        const STEP: i128 = 97_500_000;
        const CHARGE: i64 = 9_750_000;
        // `MM_130 − U + 29 · CHARGE`: puts equity at `k* − 1` exactly on maintenance.
        const M: i64 = 199_416_666;
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, M, 5, USER_WALLET as i64);
        // A well-collateralised short: equity at $130 is 700M, so it stays healthy at every tick.
        seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, 1_000_000_000, 5, 0);
        storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
        let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();

        // ── the closed form ──
        assert_eq!(STEP, MARK_130 as i128 * crate::math::MAX_FUNDING_RATE as i128);
        let u = N_130 - ENTRY_VALUE;
        assert_eq!(maintenance_margin(&market.tiers, N_130).unwrap(), MM_130);
        assert!(u >= MM_130, "profit-supported: the old clamp kept equity ≥ U > MM forever");
        let threshold = M + u - MM_130;
        assert_eq!(threshold, 282_750_000);
        assert_eq!(threshold, 29 * CHARGE, "the threshold sits exactly on an epoch");
        let k_star = threshold / CHARGE + 1;
        assert_eq!(k_star, 30);
        for k in [k_star - 1, k_star] {
            // One floor over the whole accrued delta is exactly k epochs' charge here.
            assert_eq!(
                calc_funding_payment(QTY, STEP * k as i128, 0, PRICE_DECIMALS).unwrap(),
                -CHARGE * k
            );
        }
        assert_eq!(M + u - CHARGE * (k_star - 1), MM_130, "k* − 1: equity EXACTLY MM");
        assert_eq!(M + u - CHARGE * k_star, 206_916_666, "k*: below MM, still solvent");

        let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
        assert_eq!(value_before, 1_249_416_666);

        for k in 1..k_star {
            set_funding_index(&mut ctx, STEP * k as i128);
            oracle_tick(&mut ctx, MARK_130, 15 * (k as u64 + 1) + 1);
            assert_eq!(storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(), MARK_130);
            let p = position(&mut ctx, ALICE);
            assert_eq!(
                (p.amount, p.v_quote_balance, p.margin, p.last_funding_index),
                (QTY, -ENTRY_VALUE, M, 0),
                "epoch {k}: F = {} ≤ {threshold} — above maintenance, and the healthy sweep \
                 persisted nothing",
                CHARGE * k
            );
        }
        // The positive witness at `k* − 1`: equity == MM is HEALTHY, so a manual liquidation is
        // refused by the predicate itself (and, like the sweep, persists nothing).
        let err = liquidate(&mut ctx, ALICE).unwrap_err();
        assert!(err.to_string().contains("above maintenance margin"), "{err}");
        let p = position(&mut ctx, ALICE);
        assert_eq!((p.amount, p.margin, p.last_funding_index), (QTY, M, 0));

        set_funding_index(&mut ctx, STEP * k_star as i128);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        oracle_tick(&mut ctx, MARK_130, 15 * (k_star as u64 + 1) + 1);
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let alice = position(&mut ctx, ALICE);
        assert_eq!(
            (alice.amount, alice.v_quote_balance, alice.margin),
            (0, 0, 0),
            "epoch k* = {k_star}: funding alone liquidated it, and the close released the debt"
        );
        // Close at the $130 mark: −93_083_334 of margin (the debt `M − 292.5M`) + 300M realized
        // = +206_916_666.
        assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64 + 206_916_666);
        assert_eq!(signed_wallet(&mut ctx, ALICE), 256_916_666);
        let alice_funding: Vec<i64> = decode_events::<FundingSettled>(&logs)
            .into_iter()
            .filter(|f| f.user == ALICE)
            .map(|f| f.amount)
            .collect();
        assert_eq!(alice_funding, vec![-CHARGE * k_star], "30 epochs settled ONCE, one floor");
        let adl = decode_events::<Adl>(&logs);
        assert_eq!(adl.len(), 1);
        assert_eq!(
            (adl[0].adlUser, adl[0].qty, adl[0].price),
            (KEEPER, QTY as u64, MARK_130),
            "solvent residual → ADL at MARK"
        );
        // KEEPER: margin 1e9 + 292.5M received funding, closed at mark: −300M realized.
        let keeper = position(&mut ctx, KEEPER);
        assert_eq!((keeper.amount, keeper.margin), (0, 0));
        assert_eq!(keeper.last_funding_index, STEP * k_star as i128);
        assert_eq!(signed_wallet(&mut ctx, KEEPER), 992_500_000);
        assert_eq!(
            conservation_sum(&mut ctx, &[ALICE, KEEPER]),
            value_before,
            "funding was zero-sum between the two and every leg conserved"
        );
        assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0, "no fund involvement");
    }

    // ── C2 / C3: closes net the debt against the PnL ───────────────────────────────────────────

    /// **C2 — a full voluntary close of a debt-laden long, in profit, through a real fill.**
    ///
    /// Long 10 @ $100, silo 200M, a 250M funding charge PERSISTED first → margin −50M (`D`), the
    /// stored `Σ pos.margin` −50M with it. Then ALICE sells all 10 @ $130 to MAKER at 10 bps
    /// (fee 1_300_000). `apply_position_fill` releases the margin SIGNED: credit
    /// `= −50M + 300M = 250M`, so `wallet += realized − D − fee = 300M − 50M − 1.3M = 248.7M`, margin
    /// ends EXACTLY 0 (the flat ⇒ margin == 0 invariant `save_position` asserts), and the fund does
    /// not move. Under the clamp this replaced, the close credited the full 300M and left −50M on a
    /// flat position.
    #[test]
    fn c2_a_full_close_in_profit_nets_the_funding_debt_against_the_realized_pnl() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        set_funding_index(&mut ctx, INDEX_250M);
        let settled = persist_alice_funding(&mut ctx);
        assert_eq!(settled.margin, -50_000_000);
        assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), -50_000_000);

        set_taker_bps(&mut ctx, ALICE, 10);
        storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
        place_maker_order(&mut ctx, BUY, MARK_130, QTY as u64);
        let users = [ALICE, MAKER, ADMIN];
        let value_before = conservation_sum(&mut ctx, &users);
        let wallet_before = signed_wallet(&mut ctx, ALICE);

        let logs = sell_as_alice(&mut ctx, MARK_130, QTY as u64);

        let trade = decode_events::<Trade>(&logs);
        assert_eq!(trade.len(), 1);
        assert_eq!(
            (trade[0].taker, trade[0].takerFee, trade[0].takerRealizedPnl),
            (ALICE, 1_300_000, 300_000_000)
        );
        assert_eq!(
            signed_wallet(&mut ctx, ALICE) - wallet_before,
            300_000_000 - 50_000_000 - 1_300_000,
            "wallet += realized − D − fee, exactly"
        );
        let alice = position(&mut ctx, ALICE);
        assert_eq!((alice.amount, alice.v_quote_balance, alice.margin), (0, 0, 0));
        assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), 0, "the debt left Σ too");
        assert_eq!(assert_sigma_margin_consistent(&mut ctx, MAKER), N_130, "MAKER opened at 1×");
        let row = decode_events::<PositionChanged>(&logs)
            .into_iter()
            .find(|r| r.user == ALICE)
            .unwrap();
        assert_eq!(
            (row.margin, row.realizedPnl, row.closedQuantity),
            (0, 300_000_000, QTY as u64),
            "realizedPnl stays GROSS — the debt is a margin release, not PnL"
        );
        assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 80_000_000, "fund untouched");
        assert!(decode_events::<InsuranceFundChanged>(&logs).is_empty());
        assert_eq!(
            conservation_sum(&mut ctx, &users),
            value_before,
            "Σ(wallet + margin + vQuote) + fund + fee recipient conserved across the close"
        );
    }

    /// **C3 — a 50% close, then the rest, and separately a flip, each on a stored funding debt.**
    ///
    /// Margin −50_000_001 (a 250_000_001 charge on the 200M silo) — ODD, so the 50% slice is not an
    /// exact half. `remaining = trunc0(−50_000_001 · 5 / 10) = −25_000_000`, so the closing slice
    /// carries `−25_000_001`: truncation toward zero puts the extra unit of debt on the slice being
    /// closed, never on the remainder. (A floor would leave −25_000_001 behind.)
    ///
    /// * sell 5 @ $130, 10 bps (fee 650_000): realized `−500M + 650M = 150M`, credit
    ///   `−25_000_001 + 150M = 124_999_999`, wallet `50M + 124_999_999 − 650_000 = 174_349_999`; the
    ///   remainder is exactly `(5, −500M, −25_000_000)`.
    /// * sell the other 5: credit `−25_000_000 + 150M`, wallet `298_699_999`, margin 0 — the whole
    ///   50_000_001 debt collected across the two closes.
    /// * the flip (fresh fixture, leverage 2, wallet 1e9): sell 20 @ $130, fee 2_600_000 on the full
    ///   2.6e9. The closing leg is the whole old long: credit `−50_000_001 + 300M = 249_999_999`. The
    ///   opening short is funded `1.3e9 / 2 = 650M` with the fee out of it: margin `647_400_000`, NO
    ///   debt inherited; wallet `1e9 + 249_999_999 − 650M = 599_999_999`.
    ///
    /// Each operation conserves `Σ(wallet + margin + vQuote) + fund + fee recipient` exactly (the
    /// debt was persisted beforehand, so no funding is settled inside them).
    #[test]
    fn c3_a_partial_close_and_a_flip_carry_the_slices_trunc0_share_of_the_debt() {
        const INDEX: i128 = 2_500_000_010; // charge 250_000_001
        let users = [ALICE, MAKER, ADMIN];

        // ── 50% close, then the rest ──
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        set_funding_index(&mut ctx, INDEX);
        assert_eq!(persist_alice_funding(&mut ctx).margin, -50_000_001);
        set_taker_bps(&mut ctx, ALICE, 10);
        storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
        place_maker_order(&mut ctx, BUY, MARK_130, QTY as u64);
        let value_before = conservation_sum(&mut ctx, &users);

        let logs = sell_as_alice(&mut ctx, MARK_130, 5);
        let trade = decode_events::<Trade>(&logs);
        assert_eq!((trade[0].takerFee, trade[0].takerRealizedPnl), (650_000, 150_000_000));
        let alice = position(&mut ctx, ALICE);
        assert_eq!(
            (alice.amount, alice.v_quote_balance, alice.margin),
            (5, -500_000_000, -25_000_000),
            "the remainder keeps trunc0 of its share; the slice took −25_000_001"
        );
        assert_eq!(signed_wallet(&mut ctx, ALICE), 174_349_999);
        assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), -25_000_000);
        assert_eq!(conservation_sum(&mut ctx, &users), value_before, "partial close conserves");

        let logs = sell_as_alice(&mut ctx, MARK_130, 5);
        let trade = decode_events::<Trade>(&logs);
        assert_eq!((trade[0].takerFee, trade[0].takerRealizedPnl), (650_000, 150_000_000));
        let alice = position(&mut ctx, ALICE);
        assert_eq!((alice.amount, alice.v_quote_balance, alice.margin), (0, 0, 0));
        assert_eq!(signed_wallet(&mut ctx, ALICE), 298_699_999);
        assert_eq!(
            signed_wallet(&mut ctx, ALICE),
            USER_WALLET as i64 + 2 * 150_000_000 - 50_000_001 - 2 * 650_000,
            "over both closes: the whole debt, once"
        );
        assert_eq!(conservation_sum(&mut ctx, &users), value_before, "and the rest conserves");

        // ── the flip ──
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        set_wallet(&mut ctx, ALICE, 1_000_000_000);
        set_wallet(&mut ctx, MAKER, 10_000_000_000);
        save_position_with_leverage(&mut ctx, QTY, -ENTRY_VALUE, 2);
        set_funding_index(&mut ctx, INDEX);
        assert_eq!(persist_alice_funding(&mut ctx).margin, -50_000_001);
        set_taker_bps(&mut ctx, ALICE, 10);
        storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
        place_maker_order(&mut ctx, BUY, MARK_130, 2 * QTY as u64);
        let value_before = conservation_sum(&mut ctx, &users);

        let logs = sell_as_alice(&mut ctx, MARK_130, 2 * QTY as u64);
        let trade = decode_events::<Trade>(&logs);
        assert_eq!(trade.len(), 1);
        assert_eq!((trade[0].takerFee, trade[0].takerRealizedPnl), (2_600_000, 300_000_000));
        let alice = position(&mut ctx, ALICE);
        assert_eq!(
            (alice.amount, alice.v_quote_balance, alice.margin),
            (-QTY, N_130, 650_000_000 - 2_600_000),
            "the new short's silo is exactly its own IM net of the fee — the debt stayed with the \
             closed long"
        );
        assert_eq!(signed_wallet(&mut ctx, ALICE), 599_999_999);
        assert_eq!(
            signed_wallet(&mut ctx, ALICE),
            1_000_000_000 + (300_000_000 - 50_000_001) - 650_000_000
        );
        assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), 647_400_000);
        assert_eq!(conservation_sum(&mut ctx, &users), value_before, "the flip conserves");
    }

    // ── C4: a genuine shortfall at close is the only way a debt reaches the fund ─────────────────

    /// **C4 — an insolvent close of a debt-laden long: the fund pays the SHORTFALL, via bad debt.**
    ///
    /// Long 10 @ $100, silo 200M, a 250M charge pending; ALICE sells all 10 @ $102 (realized +20M).
    /// The fill settles the funding first (margin −50M), then the close: credit
    /// `−50M + 20M = −30M` → nothing left to draw → `bad_debt = 30M`, routed to the fund. The wallet
    /// does not move (isolated margin).
    ///
    /// Two rows, the fund the only difference:
    /// * fund 80M: it covers the 30M — `InsuranceFundChanged(−30M, 50M)`, no depletion;
    /// * fund 10M: it covers only 10M — `InsuranceFundChanged(−10M, 0)` and then
    ///   `InsuranceFundDepleted(MARKET_ID, 20M)`, the 20M socialised. This is the depletion path
    ///   the retired funding test used to pin; funding can no longer reach it, a close can.
    ///
    /// The contrast is the point: under the clamp this replaced, FUNDING drew 50M from the fund
    /// on the spot and the close then paid the user its 20M profit. Now the profit nets against the
    /// debt and the fund pays only the 30M nobody can — and the event comes from the close.
    ///
    /// Value over `[ALICE, MAKER, ADMIN]` + fund moves by exactly −250M — ALICE's funding, which has
    /// no receiver here (the fund's payment of the shortfall is internal) — plus, in the depletion
    /// row, the 20M write-off: socialised debt is value nobody paid, so the measured sum RISES by it.
    #[test]
    fn c4_an_insolvent_close_routes_only_the_shortfall_to_the_fund_through_bad_debt() {
        for (fund, absorbed, depleted) in
            [(80_000_000u64, 30_000_000i64, 0u64), (10_000_000, 10_000_000, 20_000_000)]
        {
            let mut ctx = make_ctx();
            setup_market(&mut ctx);
            storage::save_insurance_fund(&mut ctx, fund).unwrap();
            save_position(&mut ctx, QTY, -ENTRY_VALUE);
            set_funding_index(&mut ctx, INDEX_250M);
            storage::save_mark_price(&mut ctx, MARKET_ID, 10_200).unwrap();
            place_maker_order(&mut ctx, BUY, 10_200, QTY as u64);
            let users = [ALICE, MAKER, ADMIN];
            let value_before = conservation_sum(&mut ctx, &users);

            let logs = sell_as_alice(&mut ctx, 10_200, QTY as u64);

            let funding = decode_events::<FundingSettled>(&logs);
            assert_eq!(funding.len(), 1);
            assert_eq!((funding[0].user, funding[0].amount), (ALICE, -250_000_000));
            let trade = decode_events::<Trade>(&logs);
            assert_eq!(trade[0].takerRealizedPnl, 20_000_000);
            let new_fund = fund - absorbed as u64;
            let changed = decode_events::<InsuranceFundChanged>(&logs);
            assert_eq!(changed.len(), 1, "fund {fund}: one fund movement — the close's");
            assert_eq!((changed[0].delta, changed[0].newBalance), (-absorbed, new_fund));
            let depletion: Vec<(u64, u64)> = decode_events::<InsuranceFundDepleted>(&logs)
                .into_iter()
                .map(|d| (d.marketId, d.badDebt))
                .collect();
            if depleted == 0 {
                assert!(depletion.is_empty(), "fund {fund}: covered in full");
            } else {
                assert_eq!(
                    depletion,
                    vec![(MARKET_ID, depleted)],
                    "fund {fund}: the uncovered rest, socialised"
                );
            }
            // The stream: the taker's funding group (its row carrying the −50M debt) is published at
            // the taker's settlement, AFTER the maker's fill group, and the fund rows follow it — the
            // fund moves for the CLOSE, ahead of the close's own group (margin 0).
            let mut expected = vec![
                ("OrderPlaced", Some(ALICE)),
                ("Trade", Some(ALICE)),
                ("AccountBalanceChanged", Some(MAKER)),
                ("PositionChanged", Some(MAKER)),
                ("FundingSettled", Some(ALICE)),
                ("AccountBalanceChanged", Some(ALICE)),
                ("PositionChanged", Some(ALICE)),
                ("InsuranceFundChanged", None),
            ];
            if depleted > 0 {
                expected.push(("InsuranceFundDepleted", None));
            }
            expected.extend([
                ("AccountBalanceChanged", Some(ALICE)),
                ("PositionChanged", Some(ALICE)),
            ]);
            assert_eq!(stream_shape(&logs), expected, "fund {fund}");
            let rows: Vec<i64> = decode_events::<PositionChanged>(&logs)
                .into_iter()
                .filter(|r| r.user == ALICE)
                .map(|r| r.margin)
                .collect();
            assert_eq!(rows, vec![-50_000_000, 0], "the debt, then the flat close");

            assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), new_fund);
            assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64, "wallet untouched");
            let alice = position(&mut ctx, ALICE);
            assert_eq!((alice.amount, alice.margin), (0, 0));
            assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), 0);
            assert_eq!(
                conservation_sum(&mut ctx, &users) - value_before,
                -250_000_000 + depleted as i128,
                "fund {fund}: ALICE's unreceived funding, plus any socialised write-off"
            );
        }
    }

    /// **C4, partial — an insolvent PARTIAL close of a debt-laden long draws nothing from a
    /// remainder that is itself in debt; the slice's whole deficit is bad debt for the fund.**
    ///
    /// Long 10 @ $100, silo 200M, a 250M charge PERSISTED → margin −50M; fund 80M. ALICE sells 5 @
    /// $95 into a 5 @ $95 bid: the slice releases `−50M − trunc0(−50M · 5/10) = −25M`, realizes
    /// `−500M + 475M = −25M`, so its credit is −50M. The remainder keeps margin −25M — a debt, so
    /// the insolvent draw's `.max(0)` finds nothing to draw and the fund pays the 50M deficit
    /// (80M → 30M). The wallet never moves.
    ///
    /// Every other debt close in this module is a FULL close, where the remainder's margin is 0 and
    /// the clamp is invisible. Without it a debt remainder would be "drawn" from: cast to `u64` it
    /// wraps to ~1.8e19 and swallows the whole deficit (margin −75M, no bad debt, fund untouched —
    /// a hidden debt piled onto an insolvent position); taken signed it RAISES the margin to 0 and
    /// over-bills the fund by 25M.
    #[test]
    fn c4_a_partial_insolvent_close_draws_nothing_from_a_remainder_that_is_itself_in_debt() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        set_funding_index(&mut ctx, INDEX_250M);
        assert_eq!(persist_alice_funding(&mut ctx).margin, -50_000_000);
        storage::save_mark_price(&mut ctx, MARKET_ID, 9_500).unwrap();
        place_maker_order(&mut ctx, BUY, 9_500, 5);
        let users = [ALICE, MAKER, ADMIN];
        let value_before = conservation_sum(&mut ctx, &users);
        let wallet_before = signed_wallet(&mut ctx, ALICE);

        let logs = sell_as_alice(&mut ctx, 9_500, 5);

        let trade = decode_events::<Trade>(&logs);
        assert_eq!(trade.len(), 1);
        assert_eq!(trade[0].takerRealizedPnl, -25_000_000);
        let alice = position(&mut ctx, ALICE);
        assert_eq!(
            (alice.amount, alice.v_quote_balance, alice.margin),
            (5, -500_000_000, -25_000_000),
            "the remainder keeps exactly its own share of the debt"
        );
        let changed = decode_events::<InsuranceFundChanged>(&logs);
        assert_eq!(changed.len(), 1);
        assert_eq!((changed[0].delta, changed[0].newBalance), (-50_000_000, 30_000_000));
        assert!(decode_events::<InsuranceFundDepleted>(&logs).is_empty());
        assert_eq!(signed_wallet(&mut ctx, ALICE), wallet_before, "isolated: wallet untouched");
        assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), -25_000_000);
        assert_eq!(conservation_sum(&mut ctx, &users), value_before);
    }

    // ── C5: liquidation through the book ─────────────────────────────────────────────────────

    /// **C5 — liquidating a debt-laden long through the book: the full fill releases the debt
    /// (margin EXACTLY 0) and the clearance fee is 0.**
    ///
    /// C1's state at `k*`: long 10 @ $100, silo 200M, 292.5M of funding pending → −92.5M once
    /// settled, at a $130 mark (equity 207.5M < MM). MAKER bids all 10 at $130; the clearance rate
    /// is 5%. The close: taker fee waived, realized +300M, credit `−92.5M + 300M = 207.5M`.
    ///
    /// * The flat position holds margin 0, so `execute_liquidation_market_order`'s
    ///   `debug_assert_eq!(pos.margin, 0)` does not fire — and its write-off branch, which would
    ///   have ZEROED the debt, is not taken.
    /// * `pre_liq_margin = margin.max(0) = 0`, so `closed_margin = 0` and the clearance fee is 0:
    ///   `Liquidation.reward == 0`, the fund unchanged. A debt-driven liquidation is fee-free — and a
    ///   long with `U ≥ MM` can only BE liquidated once its margin is negative (`M − F < MM − U ≤ 0`),
    ///   so this is every funding-driven liquidation of a profitable position, not a corner. Pinned
    ///   as current behaviour.
    #[test]
    fn c5_a_book_liquidation_releases_the_debt_and_charges_no_clearance_fee() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
        m.liquidation_fee_rate_bps = 500;
        storage::save_market(&mut ctx, &m).unwrap();
        storage::save_insurance_fund(&mut ctx, 10_000_000).unwrap();
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        set_funding_index(&mut ctx, 2_925_000_000); // 292.5M
        storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
        place_maker_order(&mut ctx, BUY, MARK_130, QTY as u64);
        let users = [ALICE, MAKER, ADMIN];
        let value_before = conservation_sum(&mut ctx, &users);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        liquidate(&mut ctx, ALICE).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let funding = decode_events::<FundingSettled>(&logs);
        assert_eq!(funding.len(), 1);
        assert_eq!(funding[0].amount, -292_500_000);
        let trade = decode_events::<Trade>(&logs);
        assert_eq!(trade.len(), 1);
        assert_eq!(
            (trade[0].price, trade[0].quantity, trade[0].takerFee, trade[0].takerRealizedPnl),
            (MARK_130, QTY as u64, 0, 300_000_000)
        );
        let alice = position(&mut ctx, ALICE);
        assert_eq!(
            (alice.amount, alice.v_quote_balance, alice.margin),
            (0, 0, 0),
            "the full fill released the debt itself — nothing for the write-off to forgive"
        );
        assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64 + 207_500_000);
        let liq = decode_events::<Liquidation>(&logs);
        assert_eq!(liq.len(), 1);
        assert_eq!(liq[0].reward, 0, "pre_liq_margin clamps the −92.5M debt to 0: no fee");
        assert!(decode_events::<InsuranceFundChanged>(&logs).is_empty());
        assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 10_000_000);
        assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), 0);
        assert_eq!(
            conservation_sum(&mut ctx, &users) - value_before,
            -292_500_000,
            "the only change in system value is the funding ALICE paid (no receiver here)"
        );
    }

    // ── C6: a short whose debt outgrew its entry proceeds, in the oracle sweep ───────────────────

    /// Short 10 @ $100 (vQuote 1e9, silo 200M) that has PAID 1.3e9 of funding (index −1.3e10: a
    /// falling index charges shorts) → margin −1.1e9, credit `vQuote + margin = −100M`: underwater
    /// at every price, `calc_bankruptcy_price` 0. KEEPER is the long on the other side; the book is
    /// empty, so the whole position is an ADL residual. Mark $100, fund 1e9.
    fn debt_short_fixture(keeper_anchor: i128) -> TestCtx {
        const INDEX: i128 = -13_000_000_000;
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        storage::save_insurance_fund(&mut ctx, 1_000_000_000).unwrap();
        seed_anchored(&mut ctx, ALICE, -QTY, ENTRY_VALUE, MARGIN, USER_WALLET as i64, 0);
        seed_anchored(&mut ctx, KEEPER, QTY, -ENTRY_VALUE, MARGIN, 0, keeper_anchor);
        set_funding_index(&mut ctx, INDEX);
        assert_eq!(
            calc_funding_payment(-QTY, INDEX, 0, PRICE_DECIMALS).unwrap(),
            -1_300_000_000,
            "the short pays 1.3e9"
        );
        assert_eq!(
            calc_bankruptcy_price(-QTY, ENTRY_VALUE, MARGIN - 1_300_000_000, 0, PRICE_DECIMALS)
                .unwrap(),
            0,
            "no representable bankruptcy price (this was an `Err` → a sweep PANIC)"
        );
        ctx
    }

    /// **C6 — the sweep on a short with `vQuote + margin < 0` does not panic; ADL fills at ONE price
    /// unit under `LoserOnly`; Σ amount is conserved; the fund absorbs the loser's shortfall.**
    ///
    /// KEEPER (long 10 @ $100, silo 200M) is anchored at 0, so it is OWED the same 1.3e9 — its
    /// effective margin is 1.5e9, which is what puts it in the CLEAN tier at price 1:
    /// `eq_fill = calc_value(1, 10) − 1e9 + 1.5e9 = 100_000 + 500M ≥ 0`. (On its stored 200M it would
    /// be −799_900_000 and fall to the fallback tier — the next test.)
    ///
    /// WHALE makes that tiering OBSERVABLE: a second long 10 @ $100 with a 300M silo and nothing
    /// pending (anchored at the current index). Its equity at the mark is 300M, above KEEPER's
    /// STORED 200M, but at price 1 it is `100_000 − 1e9 + 300M < 0` → fallback tier. On effective
    /// margin KEEPER is clean and filled first, and WHALE is never reached. Judged on STORED margin
    /// both would be fallback, ranked healthiest-at-mark first — and WHALE would be the one closed.
    ///
    /// Fill 10 @ price 1 (value 100_000):
    /// * ALICE: release −1.1e9, realized `1e9 − 100_000` → credit `−100_100_000` → `bad_debt`
    ///   100_100_000 (the only debt `LoserOnly` permits); wallet untouched, margin 0.
    /// * KEEPER: funding settled first (+1.3e9 → 1.5e9), release 1.5e9, realized `−999_900_000` →
    ///   wallet `+500_100_000`, no bad debt.
    /// * fund `1e9 − 100_100_000 = 899_900_000`.
    #[test]
    fn c6_the_sweep_on_a_short_whose_debt_exceeds_its_entry_proceeds_fills_at_one_unit() {
        let mut ctx = debt_short_fixture(0);
        seed_anchored(&mut ctx, WHALE, QTY, -ENTRY_VALUE, 300_000_000, 0, -13_000_000_000);
        let whale_before = position(&mut ctx, WHALE);
        let users = [ALICE, KEEPER, WHALE];
        let value_before = conservation_sum(&mut ctx, &users);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        oracle_tick(&mut ctx, ENTRY_PRICE, 31); // must not panic
        let logs = JournalTr::take_logs(ctx.journal_mut());

        assert_eq!(storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(), ENTRY_PRICE);
        let adl = decode_events::<Adl>(&logs);
        assert_eq!(adl.len(), 1);
        assert_eq!(
            (adl[0].liquidatedUser, adl[0].adlUser, adl[0].qty, adl[0].price),
            (ALICE, KEEPER, QTY as u64, 1),
            "no representable bankruptcy price → one price unit, against the CLEAN-tier KEEPER"
        );
        assert_eq!(
            position(&mut ctx, WHALE),
            whale_before,
            "the fallback-tier WHALE is never reached"
        );
        let (alice, keeper) = (position(&mut ctx, ALICE), position(&mut ctx, KEEPER));
        assert_eq!(alice.amount + keeper.amount, 0, "Σ amount conserved");
        assert_eq!((alice.amount, alice.v_quote_balance, alice.margin), (0, 0, 0));
        assert_eq!((keeper.amount, keeper.v_quote_balance, keeper.margin), (0, 0, 0));
        assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64, "loser wallet untouched");
        assert_eq!(signed_wallet(&mut ctx, KEEPER), 500_100_000);
        assert_eq!(keeper.last_funding_index, -13_000_000_000, "winner's funding settled");

        let changed = decode_events::<InsuranceFundChanged>(&logs);
        assert_eq!(changed.len(), 1);
        assert_eq!((changed[0].delta, changed[0].newBalance), (-100_100_000, 899_900_000));
        assert!(decode_events::<InsuranceFundDepleted>(&logs).is_empty());
        let funding: Vec<(Address, i64)> = decode_events::<FundingSettled>(&logs)
            .into_iter()
            .map(|f| (f.user, f.amount))
            .collect();
        assert_eq!(funding, vec![(ALICE, -1_300_000_000), (KEEPER, 1_300_000_000)]);
        assert_eq!(
            conservation_sum(&mut ctx, &users),
            value_before,
            "funding zero-sum, the shortfall paid by the fund"
        );
    }

    /// **C6, fallback tier — what the one-price-unit fill costs a LEVERAGED solvent winner.**
    /// Pins CURRENT behaviour; read it before changing the price rule.
    ///
    /// Same short, but KEEPER is anchored at the current index (no funding owed to it), so its
    /// margin is the stored 200M. At the $100 mark it is SOLVENT and above maintenance (equity 200M
    /// ≥ 166_666_666), but at price 1 its equity is `100_000 − 1e9 + 200M = −799_900_000`, so it
    /// lands in the FALLBACK tier and fills under `BothSides`:
    /// * ALICE's shortfall 100_100_000 and KEEPER's 799_900_000 both go to the fund: 900M;
    /// * KEEPER is closed at $0.01 and loses its WHOLE 200M silo — equity it had at the mark;
    /// * system value moves by exactly ALICE's 1.3e9 of funding, which here has no receiver.
    ///
    /// A fill at the MARK instead would leave KEEPER neutral and bill the fund the loser's whole
    /// `−1.1e9` equity at mark — 200M more for the fund, 200M less taken from KEEPER. Which of those
    /// is right is a design call (`run_adl`'s fallback-tier note); this test only records the
    /// numbers the current rule produces.
    #[test]
    fn c6_a_leveraged_solvent_winner_drafted_at_one_unit_loses_its_silo_to_the_fallback_tier() {
        let mut ctx = debt_short_fixture(-13_000_000_000);
        let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        oracle_tick(&mut ctx, ENTRY_PRICE, 31);
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let adl = decode_events::<Adl>(&logs);
        assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (KEEPER, QTY as u64, 1));
        let (alice, keeper) = (position(&mut ctx, ALICE), position(&mut ctx, KEEPER));
        assert_eq!(alice.amount + keeper.amount, 0, "Σ amount conserved");
        assert_eq!((keeper.amount, keeper.margin), (0, 0));
        assert_eq!(signed_wallet(&mut ctx, KEEPER), 0, "the 200M silo is gone");
        let changed = decode_events::<InsuranceFundChanged>(&logs);
        assert_eq!(changed.len(), 1);
        assert_eq!(
            (changed[0].delta, changed[0].newBalance),
            (-900_000_000, 100_000_000),
            "loser 100_100_000 + winner 799_900_000"
        );
        assert_eq!(
            conservation_sum(&mut ctx, &[ALICE, KEEPER]) - value_before,
            -1_300_000_000,
            "value moves by exactly the unreceived funding"
        );
    }

    /// **C6, the long mirror — an insolvent debt-laden LONG ADLs at a bankruptcy price ABOVE its
    /// entry, and the side that RECEIVED the unpaid funding is clawed back; the fund is not
    /// touched.**
    ///
    /// ALICE long 10 @ $100 and KEEPER short 10 @ $100, 200M silos, both anchored at 0; index
    /// +2.5e9, so ALICE owes 250M and KEEPER is owed 250M. At an unchanged $100 mark ALICE's settled
    /// margin is −50M, equity −50M: insolvent with no price move at all. `P_b =
    /// ⌈1.05e9 / 1e5⌉ = 10_500` — $105, ABOVE her entry. KEEPER's effective margin is 450M, equity
    /// at `P_b` `−1.05e9 + 1e9 + 450M = 400M` → clean tier, `Forbid`.
    ///
    /// Fill 10 @ $105: ALICE credit `−50M + 50M = 0` (wallet unchanged, no bad debt); KEEPER is
    /// settled +250M then realizes −50M: wallet `450M − 50M = 400M`. It keeps 200M of the 250M it was
    /// owed — the 50M ALICE could not pay comes back out of the side that was paid it. Value is
    /// conserved (450M) and the fund never moves.
    #[test]
    fn c6_an_insolvent_debt_laden_long_adls_above_entry_and_the_receiver_is_clawed_back() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
        seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 5, 0);
        set_funding_index(&mut ctx, INDEX_250M);
        assert_eq!(
            calc_bankruptcy_price(QTY, -ENTRY_VALUE, -50_000_000, 0, PRICE_DECIMALS).unwrap(),
            10_500
        );
        let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
        assert_eq!(value_before, 450_000_000);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        oracle_tick(&mut ctx, ENTRY_PRICE, 31); // the mark does not move — funding alone
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let adl = decode_events::<Adl>(&logs);
        assert_eq!(adl.len(), 1);
        assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (KEEPER, QTY as u64, 10_500));
        let paid: Vec<(Address, i64)> = decode_events::<FundingSettled>(&logs)
            .into_iter()
            .map(|f| (f.user, f.amount))
            .collect();
        assert_eq!(paid, vec![(ALICE, -250_000_000), (KEEPER, 250_000_000)]);
        let (alice, keeper) = (position(&mut ctx, ALICE), position(&mut ctx, KEEPER));
        assert_eq!((alice.amount, alice.margin, keeper.amount, keeper.margin), (0, 0, 0, 0));
        assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64, "credit exactly 0");
        assert_eq!(signed_wallet(&mut ctx, KEEPER), 400_000_000);
        assert!(decode_events::<InsuranceFundChanged>(&logs).is_empty(), "scheme X: no fund");
        assert_eq!(conservation_sum(&mut ctx, &[ALICE, KEEPER]), value_before);
    }

    /// **C6 — a debt short's residual goes to the CLEAN winner under `LoserOnly`, not to a
    /// fallback-tier holder ahead of it.**
    ///
    /// `debt_short_fixture(0)` (ALICE short, credit −100M, no representable `P_b`; KEEPER a long
    /// OWED 1.3e9 → clean at price 1) plus BOB2, a long 10 @ $110 with an empty silo and nothing
    /// pending: equity at the $100 mark −100M → fallback tier. `p_b == 0` selects `LoserOnly`, so
    /// KEEPER's trial accepts the loser's shortfall and BOB2 is never reached; the fund pays only
    /// ALICE's 100_100_000.
    ///
    /// This is the ENGINE-level witness for the `p_b == 0` → `LoserOnly` branch (the fixture's own
    /// `calc_bankruptcy_price == 0` pin is a constant). If the debt short got any price ≥ 1
    /// instead, the policy would stay `Forbid`: KEEPER's trial would be rejected for the LOSER's
    /// bad debt and re-queued behind BOB2, who would be closed under `BothSides` and bill the fund
    /// BOB2's own ~1.1e9 on top. Manual `liquidate`, not the sweep, so BOB2 is not itself swept.
    #[test]
    fn c6_a_debt_short_adls_against_the_clean_winner_under_loser_only() {
        const BOB2: Address = address!("b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2");
        let mut ctx = debt_short_fixture(0);
        seed_anchored(&mut ctx, BOB2, QTY, -1_100_000_000, 0, 0, -13_000_000_000);
        let bob2_before = position(&mut ctx, BOB2);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        liquidate(&mut ctx, ALICE).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let adl = decode_events::<Adl>(&logs);
        assert_eq!(adl.len(), 1);
        assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (KEEPER, QTY as u64, 1));
        assert_eq!(position(&mut ctx, BOB2), bob2_before, "the fallback holder is never reached");
        let changed = decode_events::<InsuranceFundChanged>(&logs);
        assert_eq!(changed.len(), 1);
        assert_eq!((changed[0].delta, changed[0].newBalance), (-100_100_000, 899_900_000));
    }

    // ── C7: ADL counterparties carrying UNSETTLED funding ──────────────────────────────────────

    /// **C7 — an ADL winner's pending funding is settled at its fill, both directions.**
    ///
    /// The audit probe: ALICE long 10 @ $100 and KEEPER short 10 @ $100, both 200M silos, both
    /// anchored at 0; the index moves ±75M (a 7.5M transfer between them); the index crashes to $75.
    /// ALICE is insolvent and her whole position ADLs against KEEPER at her bankruptcy price.
    ///
    /// | index | ALICE margin | `P_b` | KEEPER effective margin | KEEPER wallet |
    /// |-------|--------------|-------|-------------------------|---------------|
    /// | +75M  | 192.5M       | 8_075 | 207.5M (owed +7.5M)     | 400M          |
    /// | −75M  | 207.5M       | 7_925 | 192.5M (owes −7.5M)     | 400M          |
    ///
    /// KEEPER's wallet is `effective margin + (1e9 − value(P_b, 10))` = 400M either way. Before the
    /// fix KEEPER's funding was never settled — the full close re-anchored a flat position with no
    /// payment — so it got 392.5M (7.5M destroyed) or 407.5M (7.5M minted). Now its index is
    /// re-anchored, its `FundingSettled` is published ahead of the `Adl`, and total value is
    /// conserved exactly (450M).
    #[test]
    fn c7_an_adl_winners_pending_funding_is_settled_at_the_fill_in_both_directions() {
        for (index, alice_margin, p_b, keeper_payment) in [
            (75_000_000i128, 192_500_000i64, 8_075u64, 7_500_000i64),
            (-75_000_000, 207_500_000, 7_925, -7_500_000),
        ] {
            let mut ctx = make_ctx();
            setup_market(&mut ctx);
            seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
            seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 5, 0);
            set_funding_index(&mut ctx, index);
            assert_eq!(
                calc_bankruptcy_price(QTY, -ENTRY_VALUE, alice_margin, 0, PRICE_DECIMALS).unwrap(),
                p_b
            );
            let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
            assert_eq!(value_before, 450_000_000);
            let _ = JournalTr::take_logs(ctx.journal_mut());

            oracle_tick(&mut ctx, 7_500, 31);
            let logs = JournalTr::take_logs(ctx.journal_mut());

            let adl = decode_events::<Adl>(&logs);
            assert_eq!(adl.len(), 1, "index {index}");
            assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (KEEPER, QTY as u64, p_b));
            let keeper = position(&mut ctx, KEEPER);
            assert_eq!((keeper.amount, keeper.margin), (0, 0), "index {index}");
            assert_eq!(keeper.last_funding_index, index, "index {index}: winner re-anchored");
            assert_eq!(signed_wallet(&mut ctx, KEEPER), 400_000_000, "index {index}");
            assert_eq!(position(&mut ctx, ALICE).amount, 0);
            assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64, "index {index}");

            let shape = stream_shape(&logs);
            let keeper_funding = shape
                .iter()
                .position(|e| *e == ("FundingSettled", Some(KEEPER)))
                .expect("the winner's funding is published");
            let adl_at = shape.iter().position(|(n, _)| *n == "Adl").unwrap();
            assert!(keeper_funding < adl_at, "index {index}: funding → ADL, in that order");
            let paid: Vec<(Address, i64)> = decode_events::<FundingSettled>(&logs)
                .into_iter()
                .map(|f| (f.user, f.amount))
                .collect();
            assert_eq!(paid, vec![(ALICE, -keeper_payment), (KEEPER, keeper_payment)]);
            assert_eq!(
                conservation_sum(&mut ctx, &[ALICE, KEEPER]),
                value_before,
                "index {index}: no funding destroyed or minted by the ADL"
            );
            assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0);
        }
    }

    /// **C7 — a counterparty whose STORED equity is positive but whose EFFECTIVE equity (pending
    /// debt included) is ≤ 0 is FALLBACK tier, not clean tier.**
    ///
    /// ALICE long 10 @ $100 (200M) at a $75 mark: equity −50M, `P_b = 8_000`. No clean
    /// counterparty exists; two shorts, both fallback:
    /// * WHALE @ $100, silo 100M, anchored at 0 under index −3.6e9, so it OWES `3.6e9 / 10 = 360M`:
    ///   effective margin −260M, effective equity at mark `−750M + 1e9 − 260M = −10M`. On its STORED
    ///   margin it reads +350M at mark and +300M at `P_b` — clean.
    /// * BOB2 @ $50 (vQuote 500M), silo 100M, nothing pending: equity at mark −150M.
    ///
    /// The fallback tier fills HEALTHIEST first, so on effective equity WHALE (−10M) goes before
    /// BOB2 (−150M): it is closed at $80 under `BothSides`, its funding settled first (margin −260M),
    /// credit `−260M + 1e9 − 800M = −60M` → 60M of winner bad debt to the fund; BOB2 is never
    /// reached. Judged on STORED margin WHALE would instead be tried as CLEAN, fail the bad-debt-free
    /// trial, and be re-queued BEHIND BOB2 — who would then be the one closed (and bill the fund
    /// 200M). System value moves by exactly WHALE's 360M of unreceived funding.
    #[test]
    fn c7_a_counterparty_solvent_only_on_stored_margin_is_fallback_not_clean_tier() {
        const INDEX: i128 = -3_600_000_000;
        const BOB2: Address = address!("b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2");
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        storage::save_insurance_fund(&mut ctx, 1_000_000_000).unwrap();
        seed_anchored(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, USER_WALLET as i64, INDEX);
        seed_anchored(&mut ctx, WHALE, -QTY, ENTRY_VALUE, 100_000_000, 0, 0);
        seed_anchored(&mut ctx, BOB2, -QTY, 500_000_000, 100_000_000, 0, INDEX);
        set_funding_index(&mut ctx, INDEX);
        storage::save_mark_price(&mut ctx, MARKET_ID, 7_500).unwrap();
        let users = [ALICE, WHALE, BOB2];
        let value_before = conservation_sum(&mut ctx, &users);
        let bob2_before = position(&mut ctx, BOB2);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        liquidate(&mut ctx, ALICE).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let adl = decode_events::<Adl>(&logs);
        assert_eq!(adl.len(), 1);
        assert_eq!(
            (adl[0].adlUser, adl[0].qty, adl[0].price),
            (WHALE, QTY as u64, 8_000),
            "WHALE is fallback tier on its EFFECTIVE equity, and the healthiest of it"
        );
        assert_eq!(position(&mut ctx, BOB2), bob2_before, "BOB2 never reached");
        let whale = position(&mut ctx, WHALE);
        assert_eq!((whale.amount, whale.margin, whale.last_funding_index), (0, 0, INDEX));
        assert_eq!(signed_wallet(&mut ctx, WHALE), 0);
        assert_eq!((position(&mut ctx, ALICE).amount, signed_wallet(&mut ctx, ALICE)), (0, 50_000_000));
        let changed = decode_events::<InsuranceFundChanged>(&logs);
        assert_eq!(changed.len(), 1);
        assert_eq!((changed[0].delta, changed[0].newBalance), (-60_000_000, 940_000_000));
        assert_eq!(conservation_sum(&mut ctx, &users) - value_before, -360_000_000);
    }

    /// **C7 — clean-tier counterparties are RANKED on effective equity.**
    ///
    /// ALICE as above (`P_b = 8_000` at a $75 mark), index +1.5e9 (rising: shorts are OWED). Two
    /// clean shorts @ $100, uPnL 250M each:
    /// * KEEPER, silo 200M, nothing pending: equity 450M, ROE `250 / 450`;
    /// * WHALE, silo 100M, anchored at 0 → owed 150M: effective equity 500M, ROE `250 / 500` — but
    ///   on STORED margin 350M, ROE `250 / 350`, the highest.
    ///
    /// Ranked on effective equity, KEEPER goes first and absorbs all 10 (wallet `200M + 200M`);
    /// WHALE is untouched, anchor and all. The control (WHALE anchored too, nothing pending, so
    /// stored == effective) proves the fixture discriminates: WHALE goes first, wallet
    /// `100M + 200M`, and KEEPER is untouched.
    #[test]
    fn c7_clean_tier_counterparties_are_ranked_on_effective_equity() {
        const INDEX: i128 = 1_500_000_000;
        for (whale_anchor, first, wallet) in
            [(0i128, KEEPER, 400_000_000i64), (INDEX, WHALE, 300_000_000)]
        {
            let mut ctx = make_ctx();
            setup_market(&mut ctx);
            seed_anchored(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, USER_WALLET as i64, INDEX);
            seed_anchored(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 0, INDEX);
            seed_anchored(&mut ctx, WHALE, -QTY, ENTRY_VALUE, 100_000_000, 0, whale_anchor);
            set_funding_index(&mut ctx, INDEX);
            storage::save_mark_price(&mut ctx, MARKET_ID, 7_500).unwrap();
            let other = if first == KEEPER { WHALE } else { KEEPER };
            let other_before = position(&mut ctx, other);
            let _ = JournalTr::take_logs(ctx.journal_mut());

            liquidate(&mut ctx, ALICE).unwrap();
            let logs = JournalTr::take_logs(ctx.journal_mut());

            let adl = decode_events::<Adl>(&logs);
            assert_eq!(adl.len(), 1, "WHALE anchor {whale_anchor}: one counterparty takes all");
            assert_eq!((adl[0].adlUser, adl[0].price), (first, 8_000), "anchor {whale_anchor}");
            assert_eq!(position(&mut ctx, first).amount, 0);
            assert_eq!(signed_wallet(&mut ctx, first), wallet, "anchor {whale_anchor}");
            assert_eq!(position(&mut ctx, other), other_before, "anchor {whale_anchor}");
            assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0, "Forbid: no fund");
        }
    }

    /// **C7 — a PARTIALLY filled clean winner carrying a funding DEBT: the remainder keeps its
    /// trunc0 share of (stored + pending) and is re-anchored, so nothing is settled twice.**
    ///
    /// ALICE long 10 @ $100 (200M, anchored) at a $75 mark: equity −50M, `P_b = 8_000`. Index
    /// −1.5e9 (falling: shorts PAY). WHALE short 20 @ $100 (vQuote 2e9), silo 100M, anchored at 0,
    /// so it owes `20 · 1.5e9 / 100 = 300M`: effective margin −200M — a DEBT — yet effective equity
    /// at mark `−1.5e9 + 2e9 − 200M = 300M` and at `P_b` `−1.6e9 + 2e9 − 200M = 200M ≥ 0` → CLEAN
    /// tier (ROE `500M / 300M > 1`). It takes 10 of its 20 at $80 (value 800M):
    /// * WHALE: funding settled (margin −200M), then the half close releases
    ///   `−200M − trunc0(−200M · 10/20) = −100M`, realizes `1e9 − 800M = 200M` → credit +100M to the
    ///   wallet; the remainder is `(−10, 1e9, −100M)` anchored at the index — half the debt stays
    ///   with half the position, and the signed release ran on the WINNER leg;
    /// * ALICE: realized `800M − 1e9 = −200M`, release 200M → credit 0, wallet unchanged, no bad
    ///   debt, the fund untouched (`Forbid`).
    ///
    /// Afterwards WHALE has nothing pending (`compute_funding_settlement` → `None`), so a later
    /// settle cannot charge the 300M again. System value moves by exactly WHALE's unreceived 300M.
    /// WHALE, not KEEPER: `liquidate()` calls as KEEPER.
    #[test]
    fn c7_a_partially_filled_clean_winner_in_funding_debt_keeps_its_share_and_is_re_anchored() {
        const INDEX: i128 = -1_500_000_000;
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_anchored(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, USER_WALLET as i64, INDEX);
        seed_anchored(&mut ctx, WHALE, -2 * QTY, 2 * ENTRY_VALUE, 100_000_000, 0, 0);
        set_funding_index(&mut ctx, INDEX);
        storage::save_mark_price(&mut ctx, MARKET_ID, 7_500).unwrap();
        assert_eq!(
            calc_funding_payment(-2 * QTY, INDEX, 0, PRICE_DECIMALS).unwrap(),
            -300_000_000
        );
        assert_eq!(
            calc_bankruptcy_price(QTY, -ENTRY_VALUE, MARGIN, 0, PRICE_DECIMALS).unwrap(),
            8_000
        );
        let users = [ALICE, WHALE, KEEPER];
        let value_before = conservation_sum(&mut ctx, &users);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        liquidate(&mut ctx, ALICE).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let adl = decode_events::<Adl>(&logs);
        assert_eq!(adl.len(), 1);
        assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (WHALE, QTY as u64, 8_000));
        let paid: Vec<(Address, i64)> = decode_events::<FundingSettled>(&logs)
            .into_iter()
            .map(|f| (f.user, f.amount))
            .collect();
        assert_eq!(paid, vec![(WHALE, -300_000_000)], "the winner's whole pending debt, once");
        let whale = position(&mut ctx, WHALE);
        assert_eq!(
            (whale.amount, whale.v_quote_balance, whale.margin, whale.last_funding_index),
            (-QTY, ENTRY_VALUE, -100_000_000, INDEX),
            "the remainder carries half of (stored + pending), re-anchored"
        );
        assert_eq!(signed_wallet(&mut ctx, WHALE), 100_000_000);
        assert_eq!(assert_sigma_margin_consistent(&mut ctx, WHALE), -100_000_000);
        assert_eq!(position(&mut ctx, ALICE).amount, 0);
        assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64);
        assert!(decode_events::<InsuranceFundChanged>(&logs).is_empty(), "Forbid: no fund");
        assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0);
        assert_eq!(conservation_sum(&mut ctx, &users) - value_before, -300_000_000);

        // Nothing left to settle: the 300M is not charged a second time.
        let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
        let mut pos = whale.clone();
        let pending =
            crate::funding::compute_funding_settlement(&mut ctx, WHALE, &market, &mut pos, None)
                .unwrap();
        assert!(pending.is_none());
        assert_eq!(pos, whale);
    }

    /// Short ALICE 1 @ $100 (vQuote 1e8, silo 2e7, anchored — nothing pending): `P_b = $120`, and at
    /// a $130 mark she is insolvent. KEEPER is a long 3 whose EFFECTIVE margin is −1 (stored 2, owes
    /// 3 at index 100) and whose vQuote is `−(3·value($120,1) − 2)`: equity at `P_b` is +1, at mark
    /// +30_000_001 → CLEAN tier. But a 1-of-3 slice under trunc0 carries `−ceil(1/3) = −1` of margin
    /// and `−ceil(359_999_998/3) = −120M` of vQuote against +120M of proceeds: credit −1 with nothing
    /// left to draw → winner bad debt 1 → the `Forbid` trial is REJECTED and KEEPER re-queued as
    /// fallback. (That rejection is a trunc0 artifact — see `run_adl`'s re-queue note.)
    fn rejected_trial_fixture(ctx: &mut TestCtx) {
        const INDEX: i128 = 100;
        setup_market(ctx);
        storage::save_insurance_fund(ctx, 1_000_000_000).unwrap();
        seed_anchored(ctx, ALICE, -1, 100_000_000, 20_000_000, USER_WALLET as i64, INDEX);
        seed_anchored(ctx, KEEPER, 3, -359_999_998, 2, 0, 0);
        set_funding_index(ctx, INDEX);
        storage::save_mark_price(ctx, MARKET_ID, 13_000).unwrap();
        assert_eq!(calc_funding_payment(3, INDEX, 0, PRICE_DECIMALS).unwrap(), -3);
        assert_eq!(
            calc_bankruptcy_price(-1, 100_000_000, 20_000_000, 0, PRICE_DECIMALS).unwrap(),
            12_000
        );
    }

    /// **C7 — a REJECTED ADL trial publishes no funding for its winner; the fallback retry
    /// settles it ONCE.**
    ///
    /// KEEPER's clean-tier trial is rejected (`rejected_trial_fixture`), it is re-queued as fallback
    /// and filled under `BothSides` (winner bad debt 1). Its funding must be published once, by the
    /// accepted fill. Publishing it at the rejected trial would emit `FundingSettled(KEEPER, −3)`
    /// twice — the retry reloads the stored, un-anchored position — and an indexer summing
    /// `FundingSettled.amount` would charge KEEPER 6 for a 3 settlement.
    #[test]
    fn c7_a_rejected_trial_publishes_no_winner_funding_and_the_retry_settles_it_once() {
        let mut ctx = make_ctx();
        rejected_trial_fixture(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        liquidate(&mut ctx, ALICE).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let adl = decode_events::<Adl>(&logs);
        assert_eq!(adl.len(), 1);
        assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (KEEPER, 1, 12_000));
        let keeper_funding: Vec<i64> = decode_events::<FundingSettled>(&logs)
            .into_iter()
            .filter(|f| f.user == KEEPER)
            .map(|f| f.amount)
            .collect();
        assert_eq!(keeper_funding, vec![-3], "settled ONCE — the rejected trial published nothing");
        let keeper = position(&mut ctx, KEEPER);
        assert_eq!(
            (keeper.amount, keeper.v_quote_balance, keeper.margin, keeper.last_funding_index),
            (2, -239_999_998, 0, 100)
        );
        let changed = decode_events::<InsuranceFundChanged>(&logs);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].delta, -1, "the winner's 1-unit trunc0 dust, under BothSides");
    }

    /// **C7 — a REJECTED ADL trial that is never retried publishes no ghost funding.**
    ///
    /// `rejected_trial_fixture` plus BOB2, a long 1 @ $140 (silo 0, anchored) — bankrupt at the
    /// mark, so FALLBACK tier, queued ahead of the re-queued KEEPER. BOB2 absorbs the whole residual
    /// under `BothSides`; KEEPER is never retried and its stored position is untouched. The stream
    /// must carry no `FundingSettled` and no `PositionChanged` for KEEPER: publishing at the trial
    /// would show a settlement (and a row with margin −1) for a position whose stored margin is
    /// still 2 and whose anchor never moved.
    #[test]
    fn c7_a_rejected_trial_never_retried_publishes_no_ghost_funding() {
        const BOB2: Address = address!("b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2");
        let mut ctx = make_ctx();
        rejected_trial_fixture(&mut ctx);
        seed_anchored(&mut ctx, BOB2, 1, -140_000_000, 0, 0, 100);
        let keeper_before = position(&mut ctx, KEEPER);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        liquidate(&mut ctx, ALICE).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let adl = decode_events::<Adl>(&logs);
        assert_eq!(adl.len(), 1);
        assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (BOB2, 1, 12_000));
        assert_eq!(position(&mut ctx, KEEPER), keeper_before, "KEEPER never filled");
        assert!(
            decode_events::<FundingSettled>(&logs).iter().all(|f| f.user != KEEPER),
            "no funding event for a winner whose trial was rejected and never retried"
        );
        assert!(
            decode_events::<PositionChanged>(&logs).iter().all(|r| r.user != KEEPER),
            "and no position row for it either"
        );
    }

    // ── C8: margin add / remove on a debt ────────────────────────────────────────────────────

    /// **C8 — `addPositionMargin` repays a funding debt and can lift the position back above
    /// maintenance; the boundary is exact.**
    ///
    /// Long 10 @ $100 at a $100 mark, silo 200M, a 250M charge pending: once settled, margin −50M,
    /// equity −50M against `MM = 166_666_666`. Adding `X` gives equity `X − 50M`, so the smallest
    /// add that clears maintenance is `216_666_666` (equity == MM, the predicate is `>=`). One less
    /// leaves it liquidatable. The add settles the funding first and both land: margin `X − 50M`,
    /// wallet `−X`, the index re-anchored, and the stream is the funding group (row margin −50M)
    /// then the margin-transfer group.
    #[test]
    fn c8_add_position_margin_repays_the_debt_and_lifts_the_position_at_exactly_mm() {
        const X: u64 = 216_666_666;
        let market_tiers = MarginTiers::default();
        for (add, above) in [(X - 1, false), (X, true)] {
            let mut ctx = make_ctx();
            setup_market(&mut ctx);
            set_wallet(&mut ctx, ALICE, 500_000_000);
            save_position(&mut ctx, QTY, -ENTRY_VALUE);
            set_funding_index(&mut ctx, INDEX_250M);
            let _ = JournalTr::take_logs(ctx.journal_mut());

            add_position_margin(&mut ctx, add).unwrap();
            let logs = JournalTr::take_logs(ctx.journal_mut());

            let pos = position(&mut ctx, ALICE);
            assert_eq!(pos.margin, add as i64 - 50_000_000, "add {add}");
            assert_eq!(pos.last_funding_index, INDEX_250M);
            assert_eq!(signed_wallet(&mut ctx, ALICE), 500_000_000 - add as i64);
            assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), pos.margin);
            assert_eq!(
                is_above_maintenance_margin(
                    &market_tiers,
                    ENTRY_PRICE,
                    pos.amount,
                    pos.v_quote_balance,
                    pos.margin,
                    0,
                    PRICE_DECIMALS
                )
                .unwrap(),
                above,
                "add {add}: equity {} vs MM {MM_100}",
                pos.margin
            );
            assert_eq!(
                stream_shape(&logs)
                    .into_iter()
                    .map(|(n, _)| n)
                    .collect::<Vec<_>>(),
                vec![
                    "FundingSettled",
                    "AccountBalanceChanged",
                    "PositionChanged",
                    "PositionMarginAdjusted",
                    "AccountBalanceChanged",
                    "PositionChanged",
                ]
            );
            let rows: Vec<i64> =
                decode_events::<PositionChanged>(&logs).into_iter().map(|r| r.margin).collect();
            assert_eq!(rows, vec![-50_000_000, pos.margin], "the debt, then the repaid silo");
            let adj = decode_events::<PositionMarginAdjusted>(&logs);
            assert_eq!((adj[0].delta, adj[0].margin), (add as i64, pos.margin));
            if above {
                let err = liquidate(&mut ctx, ALICE).unwrap_err();
                assert!(err.to_string().contains("above maintenance margin"), "{err}");
            }
        }
    }

    /// **C8 — `removePositionMargin` on a STORED debt is refused by G1, and writes nothing.**
    /// Margin −50M persisted, no funding pending: `pos.margin < amount` for every `amount ≥ 1`.
    #[test]
    fn c8_remove_position_margin_on_a_negative_margin_is_refused_by_g1() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        set_funding_index(&mut ctx, INDEX_250M);
        assert_eq!(persist_alice_funding(&mut ctx).margin, -50_000_000);
        // Healthy at $130 (equity 250M ≥ MM 216_666_666) — G1 refuses on the BALANCE alone.
        let before = alice_untouched_snapshot(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let err = remove_position_margin(&mut ctx, 1).unwrap_err();
        assert!(
            matches!(&err, PerpError::Reject(m)
                if m == "removePositionMargin: insufficient position margin"),
            "{err}"
        );
        assert_eq!(alice_untouched_snapshot(&mut ctx), before);
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
    }

    // ── C9: K9 on a debt-laden position ──────────────────────────────────────────────────────

    /// **C9 — K9 refuses a TAKER open that leaves a debt-laden position under maintenance; it
    /// admits one that does not, and the new leg's margin nets against the debt.**
    ///
    /// Long 10 @ $100 at leverage 3, margin −50M stored. ALICE buys 1 more:
    /// * at $100: IM `⌊1e8 / 3⌋ = 33_333_333` → margin −16_666_667, equity −16_666_667 against
    ///   `MM = 1.1e9 / 6 = 183_333_333` → "open would breach maintenance margin", nothing written,
    ///   MAKER's ask untouched;
    /// * at $130 (control): IM 43_333_333 → margin −6_666_667 — STILL a debt — but equity
    ///   `1.43e9 − 1.13e9 − 6_666_667 = 293_333_333 ≥ 238_333_333` → admitted.
    #[test]
    fn c9_k9_refuses_a_taker_open_on_a_debt_laden_position_under_maintenance() {
        for (price, admitted) in [(ENTRY_PRICE, false), (MARK_130, true)] {
            let mut ctx = make_ctx();
            setup_market(&mut ctx);
            save_position_with_leverage(&mut ctx, QTY, -ENTRY_VALUE, 3);
            set_funding_index(&mut ctx, INDEX_250M);
            assert_eq!(persist_alice_funding(&mut ctx).margin, -50_000_000);
            storage::save_mark_price(&mut ctx, MARKET_ID, price).unwrap();
            place_maker_order(&mut ctx, SELL, price, 1);
            let before = alice_untouched_snapshot(&mut ctx);
            let _ = JournalTr::take_logs(ctx.journal_mut());

            let res = try_place_order(&mut ctx, ALICE, BUY, price, 1);
            if !admitted {
                let err = res.unwrap_err();
                assert!(
                    matches!(&err, PerpError::Reject(m)
                        if m == "placeOrder: open would breach maintenance margin"),
                    "{err}"
                );
                assert_eq!(alice_untouched_snapshot(&mut ctx), before, "a K9 refusal writes nothing");
                assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
                assert_eq!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(), vec![price]);
            } else {
                res.unwrap();
                let pos = position(&mut ctx, ALICE);
                assert_eq!(
                    (pos.amount, pos.v_quote_balance, pos.margin),
                    (QTY + 1, -1_130_000_000, -50_000_000 + 43_333_333),
                    "admitted, and the new IM netted against the debt"
                );
                assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64 - 43_333_333);
            }
        }
    }

    /// **C9 — a MAKER fill that would OPEN on a debt-laden position below maintenance is
    /// `RejectedInsolvent`: the order is cancelled (`MakerInsolvent`), the fill does not land, and
    /// the only state change is the maker's funding settle.**
    ///
    /// ALICE (long 10 @ $100, leverage 3, silo 200M) rests a bid for 1 @ $100 while nothing is
    /// pending. Then a 250M charge accrues and MAKER sells into the bid: the match settles ALICE's
    /// funding (−50M), the trial fill would leave margin `−50M + 33_333_333` and equity −16_666_667
    /// against MM 183_333_333, so K9 rejects. MAKER's sell rests instead.
    #[test]
    fn c9_k9_cancels_a_maker_open_on_a_debt_laden_position() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        save_position_with_leverage(&mut ctx, QTY, -ENTRY_VALUE, 3);
        place_order(&mut ctx, ALICE, BUY, ENTRY_PRICE, 1);
        assert_eq!(position(&mut ctx, ALICE).total_buy_qty, 1, "fixture: the bid rests");
        set_funding_index(&mut ctx, INDEX_250M);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        try_place_order(&mut ctx, MAKER, SELL, ENTRY_PRICE, 1).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());

        assert!(decode_events::<Trade>(&logs).is_empty(), "the fill did not land");
        let cancelled = decode_events::<OrderCancelled>(&logs);
        assert_eq!(cancelled.len(), 1);
        assert_eq!(
            (cancelled[0].user, cancelled[0].reason),
            (ALICE, CancelReason::MakerInsolvent as u8)
        );
        let alice = position(&mut ctx, ALICE);
        assert_eq!(
            (alice.amount, alice.v_quote_balance, alice.margin, alice.last_funding_index),
            (QTY, -ENTRY_VALUE, -50_000_000, INDEX_250M),
            "only the funding settle landed"
        );
        assert_eq!(alice.total_buy_qty, 0, "the bid is gone");
        assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64);
        let funding = decode_events::<FundingSettled>(&logs);
        assert_eq!((funding[0].user, funding[0].amount), (ALICE, -250_000_000));
        assert_eq!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(), vec![ENTRY_PRICE]);
        assert_eq!(position(&mut ctx, MAKER).amount, 0);
    }
}
