//! **Trading-fee accounting through the full call shell**: how the ONE taker fee an order pays is
//! attributed to its individual fills (`Trade.takerFee`).
//!
//! Every call goes through `run_perp_dex_call`, so the gas charge, the commit-only #23 tripwire, the
//! debug event guards in `crate::events` and the end-of-call `AccountBalanceChanged` drain are all in
//! the loop — and so are the two production `debug_assert`s this suite leans on:
//! `TakerSettlement::record_fill` (the share recorded is the share priced) and `finalize_core`
//! (Σ shares == the fee charged).
//!
//! The market is chosen so fees TRUNCATE: base 0 / price 6 decimals, tick 1, step 1, so a fill of
//! `q` at `p` has notional `n = p·q` micro-USDC exactly, and a level `5_000` micro-dollars off a
//! round price leaves half a unit of fee on the floor per fill. That is what makes the cumulative
//! split observably different from an independent per-fill floor.

use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, U256};

use crate::call::PerpOutput;
use crate::interface::IPerpDex::{placeOrderCall, setUserFeeRatesCall, Trade};
use crate::types::{AccountUpdateReason, MarginTiers, Market};
use crate::{run_perp_dex_call, storage, PERP_DEX_ADDRESS, USDC_ADDRESS};

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

/// The taker in every scenario.
const ALICE: Address = address!("1111111111111111111111111111111111111111");
/// Makers.
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
/// The fee recipient.
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const MARKET_ID: u64 = 1;
/// $100.000000 at price_decimals 6.
const MARK: u64 = 100_000_000;
/// $1e9 — margin never interferes unless a test sets the wallet itself.
const WALLET: i64 = 1_000_000_000_000_000;
const GAS: u64 = 30_000_000;

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const MKT: u8 = 1;
const GTC: u8 = 0;

// ── Fixture ──────────────────────────────────────────────────────────────────────────────────

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, BOB, CAROL, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

/// Market 1 at a $100 mark (base 0 / price 6 decimals, tick 1, step 1, no funding, the default
/// 10% fill-time band), a $1e9 perp wallet for ALICE, BOB and CAROL, and an EMPTY fee-recipient
/// wallet, so every unit the admin holds afterwards is a fee. Incidental logs discarded.
fn fresh() -> TestCtx {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    storage::save_market(
        &mut ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 0,
            price_decimals: 6,
            tick_size: 1,
            step_size: 1,
            min_quantity: 1,
            max_quantity: 1_000_000,
            max_price: 1_000_000_000_000,
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
    storage::save_mark_price(&mut ctx, MARKET_ID, MARK).unwrap();
    for u in [ALICE, BOB, CAROL] {
        let mut a = storage::load_account(&mut ctx, u).unwrap();
        a.perp_wallet_balance = WALLET;
        storage::save_account(&mut ctx, u, a, AccountUpdateReason::Adjustment).unwrap();
    }
    let _ = take_logs(&mut ctx);
    ctx
}

// ── Shell helpers ────────────────────────────────────────────────────────────────────────────

fn take_logs(ctx: &mut TestCtx) -> Vec<primitives::Log> {
    JournalTr::take_logs(ctx.journal_mut())
}

/// The `Error(string)` reason of a reverted shell output.
fn revert_reason(out: &PerpOutput) -> String {
    assert!(out.reverted, "the call did not revert");
    let b = &out.bytes;
    assert_eq!(&b[..4], &[0x08, 0xc3, 0x79, 0xa0], "Error(string) selector");
    let len = U256::from_be_slice(&b[36..68]).to::<usize>();
    String::from_utf8(b[68..68 + len].to_vec()).unwrap()
}

/// One SUCCESSFUL shell call; returns the output and exactly this call's logs.
fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> (PerpOutput, Vec<primitives::Log>) {
    let _ = take_logs(ctx);
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(!out.reverted, "unexpected revert: {}", revert_reason(&out));
    let logs = take_logs(ctx);
    (out, logs)
}

fn evs<E: SolEvent>(logs: &[primitives::Log]) -> Vec<E> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect()
}

fn place(side: u8, price: u64, qty: u64, order_type: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif: GTC,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode()
}

/// A resting GTC that crossed nothing.
fn rest(ctx: &mut TestCtx, user: Address, side: u8, price: u64, qty: u64) {
    let (_, logs) = ok(ctx, user, &place(side, price, qty, LIMIT));
    assert!(evs::<Trade>(&logs).is_empty(), "{user} {side}@{price} traded while resting");
}

/// A market order; returns its `Trade` rows.
fn take(ctx: &mut TestCtx, user: Address, side: u8, qty: u64) -> Vec<Trade> {
    let (_, logs) = ok(ctx, user, &place(side, 0, qty, MKT));
    evs::<Trade>(&logs)
}

fn set_fees(ctx: &mut TestCtx, user: Address, maker_bps: u64, taker_bps: u64) {
    ok(
        ctx,
        ADMIN,
        &setUserFeeRatesCall {
            user,
            makerFeeBps: maker_bps,
            takerFeeBps: taker_bps,
        }
        .abi_encode(),
    );
}

/// The fee recipient's signed perp wallet.
fn admin_wallet(ctx: &mut TestCtx) -> i64 {
    storage::load_account(ctx, ADMIN).unwrap().perp_wallet_balance
}

fn fee_total(ctx: &mut TestCtx) -> u64 {
    storage::load_market_fee_total(ctx, MARKET_ID).unwrap()
}

/// `calc_value(p, q)` on this market: `p·q·1e6 / (1e6·1e0)`.
fn notional(p: u64, q: u64) -> u64 {
    p * q
}

/// The maker levels a `taker_side` order crosses, touch first: three levels each `5_000`
/// micro-dollars off a round price, so every fill's 1 bp fee is `x.5` units before the floor.
fn levels(taker_side: u8) -> [u64; 3] {
    if taker_side == BUY {
        [MARK + 5_000, MARK + 15_000, MARK + 25_000]
    } else {
        [MARK - 5_000, MARK - 15_000, MARK - 25_000]
    }
}

// ── Taker fee attribution ────────────────────────────────────────────────────────────────────

/// `Trade.takerFee` is the fill's share of the ONE fee the taker pays, split by differences of
/// cumulative floors, so the shares sum to that fee EXACTLY — where the old independent per-fill
/// floor dropped up to `N − 1` units.
///
/// Fixture: three fills of notional `100_005_000`, `100_015_000`, `100_025_000` (or the bid-side
/// mirror) at a 1 bp taker rate, i.e. `10_000.5`, `10_001.5`, `10_002.5` units before the floor.
///
/// ```text
/// C_k          floor(C_k · 1/10_000)   share_k
/// 100_005_000         10_000            10_000
/// 200_020_000         20_002            10_002
/// 300_045_000         30_004            10_002     Σ = 30_004 = F
/// ```
///
/// The independent floors give `10_000 + 10_001 + 10_002 = 30_003` — one unit short of what the
/// taker was charged — so the fixture DISCRIMINATES between the two splits (asserted).
#[test]
fn taker_fee_shares_telescope_to_the_one_fee_charged() {
    for side in [BUY, SELL] {
        let mut ctx = fresh();
        set_fees(&mut ctx, ALICE, 0, 1);
        let maker_side = 1 - side;
        let lv = levels(side);
        rest(&mut ctx, BOB, maker_side, lv[0], 1);
        rest(&mut ctx, CAROL, maker_side, lv[1], 1);
        rest(&mut ctx, BOB, maker_side, lv[2], 1);
        let admin_before = admin_wallet(&mut ctx);
        let fee_total_before = fee_total(&mut ctx);

        let trades = take(&mut ctx, ALICE, side, 3);

        assert_eq!(trades.len(), 3, "side {side}: three fills");
        assert_eq!(
            trades.iter().map(|t| t.price).collect::<Vec<_>>(),
            lv.to_vec(),
            "side {side}: the walk takes the touch first"
        );
        let charged: u64 = lv.iter().map(|&p| notional(p, 1)).sum::<u64>() / 10_000;
        assert_eq!(charged, if side == BUY { 30_004 } else { 29_995 });
        let shares: Vec<u64> = trades.iter().map(|t| t.takerFee).collect();
        assert_eq!(
            shares,
            if side == BUY {
                vec![10_000, 10_002, 10_002]
            } else {
                vec![9_999, 9_999, 9_997]
            },
            "side {side}: differences of cumulative floors"
        );
        assert_eq!(shares.iter().sum::<u64>(), charged, "side {side}: Σ shares == F");
        // Makers pay 0, so the admin's delta and the fee total are the taker fee alone.
        assert_eq!(admin_wallet(&mut ctx) - admin_before, charged as i64);
        assert_eq!(fee_total(&mut ctx) - fee_total_before, charged);

        // The fixture discriminates: the independent per-fill floor loses a unit.
        let independent: u64 = lv.iter().map(|&p| notional(p, 1) / 10_000).sum();
        assert_eq!(independent + 1, charged, "side {side}: fixture must truncate");
    }
}

/// A fill whose maker is K9-rejected is never recorded, so it consumes NO share: the next accepted
/// fill's share is taken against the running total of ACCEPTED fills only, and the shares still
/// sum to the fee charged on the accepted notional.
#[test]
fn a_rejected_maker_consumes_no_taker_fee_share() {
    let mut ctx = fresh();
    set_fees(&mut ctx, ALICE, 0, 1);
    // CAROL rests the touch with a wallet that cannot fund it at all; the mark then moves so far
    // against her that the fill would open her below maintenance → K9 cancels her order.
    let lv = levels(BUY);
    rest(&mut ctx, CAROL, SELL, lv[0], 1);
    rest(&mut ctx, BOB, SELL, lv[1], 1);
    rest(&mut ctx, BOB, SELL, lv[2], 1);
    let mut a = storage::load_account(&mut ctx, CAROL).unwrap();
    a.perp_wallet_balance = 0;
    storage::save_account(&mut ctx, CAROL, a, AccountUpdateReason::Adjustment).unwrap();
    let _ = take_logs(&mut ctx);
    let admin_before = admin_wallet(&mut ctx);

    let trades = take(&mut ctx, ALICE, BUY, 2);

    assert_eq!(
        trades.iter().map(|t| t.maker).collect::<Vec<_>>(),
        vec![BOB, BOB],
        "CAROL's unfunded opening short is K9-cancelled, the taker walks on"
    );
    // Shares over the ACCEPTED fills (lv[1], lv[2]) only:
    //   C1 = 100_015_000 → 10_001        C2 = 200_040_000 → 20_004 → share 10_003
    let shares: Vec<u64> = trades.iter().map(|t| t.takerFee).collect();
    assert_eq!(shares, vec![10_001, 10_003]);
    let charged = (notional(lv[1], 1) + notional(lv[2], 1)) / 10_000;
    assert_eq!(shares.iter().sum::<u64>(), charged);
    assert_eq!(admin_wallet(&mut ctx) - admin_before, charged as i64);
}
