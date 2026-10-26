//! Cross margin P3a — cross LIQUIDATION: account-level detection and the episode
//! (`risk::cross_liquidation`), under the custody identity.
//!
//! * the episode: partial (stops at `E ≥ Σ MM`, Q6) and full closes, the IOC at the MM-share
//!   bankruptcy price clamped into the band (Q4/Q5), ADL of the rest at the mark (`E ≥ 0`) or at
//!   the bankruptcy price (`E < 0`, R5), the deficit split by MM across markets (D4), the stale-mark
//!   skip (Q10), the clearance fee on the notional closed capped at `max(0, E_after)` (Q3);
//! * cancel-all across every market of the account, isolated markets and conditional orders
//!   included, the isolated position untouched (Q2, R3);
//! * detection completeness (the account is caught on the tick of the market that moved, which
//!   closes first) and the close order;
//! * budget and continuation (`deferred_work`, resumed by the drain), the `-2023` freeze in between;
//! * manual `liquidate()`: healthy refused write-clean, a recovered flag cleared, the start market;
//! * J4: a mark of 0 and a leg the fold cannot price are skipped, never an `Err`;
//! * a randomised multi-market run: never a panic, custody after every step, and after every tick
//!   no account holding a cross position in the ticked market is left below maintenance unflagged.
//!
//! Every scripted scenario re-checks the custody identity after each step ([`Ledger::check`]).
//! Fees are 0 here so every number is exact; the clearance-fee tests set a rate explicitly.
use alloy_sol_types::{SolCall, SolError, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, Log, U256};

use crate::host::PerpHost;
use crate::{
    interface::IPerpDex::{
        drainDeferredWorkCall, liquidateCall, placeAlgoOrderCall, placeOrderCall, setLeverageCall,
        setMarginTypeCall, updateIndexPriceCall, Adl, AlgoOrderCancelled, CrossLiquidationEnded,
        CrossLiquidationStarted, Liquidation, OrderCancelled, OrderPlaced, Trade,
    },
    risk::cross_liquidation::{liquidate_cross_account, CrossLiquidationOutcome, EpisodeCaller},
    run_perp_dex_call, storage,
    types::{AccountUpdateReason, FundingState, MarginTiers, MarginType, Market, UserAccount},
    PerpOutput, PERP_DEX_ADDRESS, USDC_ADDRESS,
};

// ── Fixture ───────────────────────────────────────────────────────────────────

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
const MAKER: Address = address!("4444444444444444444444444444444444444444");
const KEEPER: Address = address!("5555555555555555555555555555555555555555");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
/// Every account that can hold value here.
const USERS: [Address; 6] = [ALICE, BOB, CAROL, MAKER, KEEPER, ADMIN];

/// Three markets on one grid: base 0 decimals, price 2 decimals, so one contract at `P` cents is
/// worth `P · 10^4` quote units. Default tier table: one bracket at 3x, `MM = |N| / 6`.
const M1: u64 = 1;
const M2: u64 = 2;
const M3: u64 = 3;
const MARKETS: [u64; 3] = [M1, M2, M3];
const USD: i64 = 1_000_000;
const WALLET: i64 = 1_000_000 * USD;

const BUY: u8 = 0;
const SELL: u8 = 1;
const GTC: u8 = 0;
const IOC: u8 = 1;
const RO: u8 = 1;
const CROSSED: u8 = 0;
const ISOLATED: u8 = 1;
/// `OrderCancelled.reason` / `AlgoOrderCancelled.reason` 3 = Liquidation.
const CANCEL_LIQUIDATION: u8 = 3;

const fn px(dollars: u64) -> u64 {
    dollars * 100
}

/// `$d` in quote units.
const fn usd(d: i64) -> i64 {
    d * USD
}

fn market(id: u64) -> Market {
    Market {
        market_id: id,
        base_decimals: 0,
        price_decimals: 2,
        tick_size: 1,
        step_size: 1,
        min_quantity: 1,
        max_quantity: 1_000_000,
        max_price: 100_000_000,
        price_update_interval: 15,
        active: true,
        funding_interval: 0,
        interest_rate: 0,
        liquidation_fee_rate_bps: 0,
        // ≥ 100%: the fill-time band is off unless a test sets one.
        price_band_bps: 10_000,
        mark_price: px(100),
        tiers: MarginTiers::default(),
        basis_mode: 0,
        deferred_work: 0,
        cumulative_funding_index: 0,
    }
}

/// Cross margin on (the test override); every account but ALICE switched to ISOLATED everywhere,
/// so ALICE is the only CROSSED account unless a test says otherwise.
fn make_ctx() -> (crate::margin_type::test_gate::Guard, TestCtx) {
    let gate = crate::margin_type::test_gate::enable();
    let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
    for a in [USDC_ADDRESS, PERP_DEX_ADDRESS].into_iter().chain(USERS) {
        JournalTr::load_account(ctx.journal_mut(), a).unwrap();
    }
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    for id in MARKETS {
        storage::save_market(&mut ctx, &market(id)).unwrap();
    }
    for u in [ALICE, BOB, CAROL, MAKER, KEEPER] {
        let a = UserAccount { perp_wallet_balance: WALLET, ..UserAccount::default() };
        storage::save_account(&mut ctx, u, a, AccountUpdateReason::Adjustment).unwrap();
    }
    storage::flush_account_snapshots(&mut ctx).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());
    for u in [BOB, CAROL, MAKER, KEEPER] {
        for m in MARKETS {
            set_type(&mut ctx, u, m, ISOLATED);
        }
    }
    for m in MARKETS {
        set_leverage(&mut ctx, ALICE, m, 3);
    }
    (gate, ctx)
}

fn call(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, 30_000_000, caller, U256::ZERO, false, ctx)
        .expect("a perp call must not hard-fail")
}

fn revert_reason(out: &PerpOutput) -> String {
    alloy_sol_types::Revert::abi_decode(&out.bytes).map(|r| r.reason).unwrap_or_default()
}

fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> Vec<Log> {
    let out = call(ctx, caller, input);
    assert!(!out.reverted, "call reverted: {}", revert_reason(&out));
    JournalTr::take_logs(ctx.journal_mut())
}

/// Runs `input` and asserts it reverted with exactly `reason`, writing nothing.
fn rejects(ctx: &mut TestCtx, caller: Address, input: &[u8], reason: &str) {
    let before = world(ctx);
    let writes = ctx.perp_write_count();
    let out = call(ctx, caller, input);
    assert!(out.reverted, "expected a revert with {reason:?}");
    assert_eq!(revert_reason(&out), reason);
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty(), "a refused call logged");
    assert_eq!(ctx.perp_write_count(), writes, "a refused call wrote");
    assert_eq!(world(ctx), before, "a refused call moved state");
}

fn world(ctx: &mut TestCtx) -> Vec<String> {
    let mut out = Vec::new();
    for u in USERS {
        out.push(format!("{:?}", storage::load_account(ctx, u).unwrap()));
        for m in MARKETS {
            out.push(format!("{:?}", pos(ctx, u, m)));
            out.push(format!("{:?}", storage::load_buy_orders(ctx, u, m).unwrap()));
            out.push(format!("{:?}", storage::load_sell_orders(ctx, u, m).unwrap()));
            out.push(format!("{:?}", storage::load_user_algo_orders(ctx, u, m).unwrap()));
        }
    }
    out.push(format!("IF {}", storage::load_insurance_fund(ctx).unwrap()));
    out
}

fn order_flags(market: u64, side: u8, price: u64, qty: u64, tif: u8, flags: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: market,
        side,
        price,
        quantity: qty,
        orderType: 0,
        tif,
        clientOrderId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

fn order(market: u64, side: u8, price: u64, qty: u64, tif: u8) -> Vec<u8> {
    order_flags(market, side, price, qty, tif, 0)
}

/// `maker` rests a GTC at `price`, `taker` crosses it with an IOC of the same size.
fn trade(ctx: &mut TestCtx, m: u64, taker: Address, maker: Address, side: u8, price: u64, qty: u64) {
    ok(ctx, maker, &order(m, 1 - side, price, qty, GTC));
    ok(ctx, taker, &order(m, side, price, qty, IOC));
}

fn rest(ctx: &mut TestCtx, user: Address, m: u64, side: u8, price: u64, qty: u64) -> [u8; 32] {
    let logs = ok(ctx, user, &order(m, side, price, qty, GTC));
    assert!(evs::<Trade>(&logs).is_empty(), "the resting order traded");
    placed_id(&logs, user)
}

fn placed_id(logs: &[Log], user: Address) -> [u8; 32] {
    evs::<OrderPlaced>(logs).iter().find(|o| o.user == user).expect("an OrderPlaced").orderId.0
}

fn set_type(ctx: &mut TestCtx, user: Address, m: u64, t: u8) {
    ok(ctx, user, &setMarginTypeCall { marketId: m, marginType: t }.abi_encode());
}

fn set_leverage(ctx: &mut TestCtx, user: Address, m: u64, leverage: u64) {
    ok(ctx, user, &setLeverageCall { marketId: m, leverage }.abi_encode());
}

/// Fixture only: moves a mark WITHOUT a tick, so no sweep runs.
fn set_mark(ctx: &mut TestCtx, m: u64, mark: u64) {
    storage::save_mark_price(ctx, m, mark).unwrap();
}

fn set_wallet(ctx: &mut TestCtx, user: Address, w: i64) {
    storage::mutate_account(ctx, user, |a| a.perp_wallet_balance = w).unwrap();
}

fn set_fee_rate(ctx: &mut TestCtx, m: u64, bps: u32) {
    let mut mk = storage::load_market(ctx, m).unwrap().unwrap();
    mk.liquidation_fee_rate_bps = bps;
    storage::save_market(ctx, &mk).unwrap();
}

fn wallet(ctx: &mut TestCtx, user: Address) -> i64 {
    storage::load_account(ctx, user).unwrap().perp_wallet_balance
}

fn flagged(ctx: &mut TestCtx, user: Address) -> bool {
    storage::load_account(ctx, user).unwrap().in_liquidation()
}

fn pos(ctx: &mut TestCtx, user: Address, m: u64) -> crate::types::PerpPosition {
    storage::load_position(ctx, user, m).unwrap()
}

fn insurance_fund(ctx: &mut TestCtx) -> u64 {
    storage::load_insurance_fund(ctx).unwrap()
}

fn fold(ctx: &mut TestCtx, user: Address) -> crate::margin_view::CrossFold {
    crate::margin_view::cross_fold(ctx, user, None, None, None).unwrap()
}

fn evs<E: SolEvent>(logs: &[Log]) -> Vec<E> {
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|l| E::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .collect()
}

/// `(liquidatedUser, adlUser, marketId, qty, price)` of every ADL fill.
fn adls(logs: &[Log]) -> Vec<(Address, Address, u64, u64, u64)> {
    evs::<Adl>(logs)
        .into_iter()
        .map(|a| (a.liquidatedUser, a.adlUser, a.marketId, a.qty, a.price))
        .collect()
}

/// An oracle tick at an explicit timestamp. The last traded price is reset first, so the mark is
/// exactly the index (`median(price1 = index, price2, contract = index)` with funding off).
fn tick_at(ctx: &mut TestCtx, m: u64, index: u64, ts: u64) -> Vec<Log> {
    storage::save_last_traded_price(ctx, m, 0).unwrap();
    let logs = ok(
        ctx,
        ADMIN,
        &updateIndexPriceCall { marketId: m, indexPrice: index, timestamp: ts }.abi_encode(),
    );
    assert_eq!(storage::load_mark_price(ctx, m).unwrap(), index, "the tick set the mark");
    logs
}

/// An oracle tick one interval after the market's last one.
fn tick(ctx: &mut TestCtx, m: u64, index: u64) -> Vec<Log> {
    let ts = storage::load_index_price_state(ctx, m).unwrap().timestamp + 15;
    tick_at(ctx, m, index, ts)
}

fn liquidate(user: Address, m: u64) -> Vec<u8> {
    liquidateCall { user, marketId: m }.abi_encode()
}

// ── The custody identity ────────────────────────────────────────────────────────

/// `Σ_users (spot + W + Σ_markets (margin + vQuote + PF)) + IF` — every term a liquidation,
/// a fee or an ADL fill moves value between — must never move; `Σ amount == 0` per market; a
/// position CROSSED in force holds no silo.
struct Ledger {
    total: i128,
}

impl Ledger {
    fn system_value(ctx: &mut TestCtx) -> i128 {
        let mut v = insurance_fund(ctx) as i128;
        for u in USERS {
            let a = storage::load_account(ctx, u).unwrap();
            let spot: U256 = a.usdc_balance.clone().into();
            v += spot.to::<u128>() as i128 + a.perp_wallet_balance as i128;
            for m in MARKETS {
                let p = pos(ctx, u, m);
                let mk = storage::load_market(ctx, m).unwrap().unwrap();
                let index = storage::load_funding_state(ctx, m).unwrap().cumulative_funding_index;
                let pf = crate::funding::pending_funding_payment(index, &p, &mk).unwrap();
                v += p.margin as i128 + p.v_quote_balance as i128 + pf as i128;
            }
        }
        v
    }

    fn open(ctx: &mut TestCtx) -> Self {
        Self { total: Self::system_value(ctx) }
    }

    fn check(&self, ctx: &mut TestCtx, at: &str) {
        for m in MARKETS {
            let net: i128 = USERS.iter().map(|&u| pos(ctx, u, m).amount as i128).sum();
            assert_eq!(net, 0, "{at}: market {m} is not zero-sum (Σ amount = {net})");
            for u in USERS {
                let p = pos(ctx, u, m);
                if crate::margin_type::effective_margin_type(&p) == MarginType::Crossed {
                    assert_eq!(p.margin, 0, "{at}: {u} is CROSSED in market {m} but holds a silo");
                }
            }
        }
        assert_eq!(Self::system_value(ctx), self.total, "{at}: CUSTODY BROKEN");
    }
}

/// ALICE: a CROSSED 3x long of 10 on M1 at $100 against BOB (ISOLATED short), with the wallet `w`
/// left after it. At a $80 mark her `E = w − $200` against `Σ MM = $133.33`.
fn long_10_on_m1(ctx: &mut TestCtx, w: i64) {
    trade(ctx, M1, ALICE, BOB, BUY, px(100), 10);
    set_wallet(ctx, ALICE, w);
}

// ── The episode: one market ─────────────────────────────────────────────────────

/// **Q6 — a PARTIAL liquidation stops as soon as the account is back at maintenance.** At $80:
/// `E = $100 < Σ MM = $133.33`. The IOC sells all 10 at the bankruptcy price — the whole share is
/// M1's, `$800 − $100` of notional, i.e. a $70 limit (band off) — and the book has 4 at $80: they
/// fill, realize −$80, and leave `E = $100` against `Σ MM = $80`: healthy, so it STOPS — no ADL,
/// the other 6 stay ALICE's, the flag is cleared. Events: `Started(E, ΣMM)` at detection, one
/// `Liquidation` (marginType 0), `Ended` with the values that ended it.
#[test]
fn a_partial_cross_liquidation_stops_once_the_account_is_back_at_maintenance() {
    let (_g, mut ctx) = make_ctx();
    long_10_on_m1(&mut ctx, usd(300));
    rest(&mut ctx, MAKER, M1, BUY, px(80), 4);
    let ledger = Ledger::open(&mut ctx);

    let logs = tick(&mut ctx, M1, px(80));
    ledger.check(&mut ctx, "after the tick");
    let close = evs::<OrderPlaced>(&logs);
    let close: Vec<&OrderPlaced> = close.iter().filter(|o| o.user == ALICE).collect();
    assert_eq!(close.len(), 1);
    assert_eq!(
        (close[0].side, close[0].price, close[0].quantity, close[0].orderType, close[0].tif),
        (SELL, px(70), 10, 0, IOC),
        "a LIMIT IOC for the whole position at the bankruptcy price"
    );
    let t = evs::<Trade>(&logs);
    assert_eq!((t.len(), t[0].quantity, t[0].price, t[0].taker), (1, 4, px(80), ALICE));
    assert!(adls(&logs).is_empty(), "stopped before ADL");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 6);
    assert_eq!(wallet(&mut ctx, ALICE), usd(220));
    let liq = evs::<Liquidation>(&logs);
    assert_eq!(liq.len(), 1);
    assert_eq!(
        (liq[0].user, liq[0].marketId, liq[0].amount, liq[0].reward, liq[0].marginType),
        (ALICE, M1, 10, 0, CROSSED)
    );
    let started = evs::<CrossLiquidationStarted>(&logs);
    assert_eq!(started.len(), 1);
    assert_eq!((started[0].user, started[0].triggerMarketId), (ALICE, M1));
    assert_eq!(started[0].crossMarginBalance, usd(100));
    assert!((133_333_333..=133_333_334).contains(&started[0].totalCrossMaintMargin));
    let ended = evs::<CrossLiquidationEnded>(&logs);
    assert_eq!(ended.len(), 1);
    assert_eq!(
        (ended[0].user, ended[0].marketId, ended[0].crossMarginBalance),
        (ALICE, M1, usd(100))
    );
    assert_eq!(ended[0].totalCrossMaintMargin, usd(80) as u64);
    assert!(!flagged(&mut ctx, ALICE));
    let f = fold(&mut ctx, ALICE);
    assert!(!f.below_cross_maintenance() && f.has_cross, "healthy, still holding the rest");

    // The next tick at the same mark leaves her alone.
    let logs = tick(&mut ctx, M1, px(80));
    assert!(evs::<Liquidation>(&logs).is_empty());
}

/// When the book absorbs the whole IOC the position is closed in one step, and the clearance fee is
/// the NOTIONAL one: 1.25% of the $800 closed = $10, under its `max(0, E_after) = $100` cap,
/// wallet → insurance fund.
#[test]
fn a_cross_liquidation_the_book_absorbs_closes_the_position_and_charges_the_notional_fee() {
    let (_g, mut ctx) = make_ctx();
    set_fee_rate(&mut ctx, M1, 125);
    long_10_on_m1(&mut ctx, usd(300));
    rest(&mut ctx, MAKER, M1, BUY, px(80), 10);
    let ledger = Ledger::open(&mut ctx);
    let if0 = insurance_fund(&mut ctx);

    let logs = tick(&mut ctx, M1, px(80));
    ledger.check(&mut ctx, "after the tick");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    assert_eq!(wallet(&mut ctx, ALICE), usd(300 - 200 - 10));
    assert_eq!(insurance_fund(&mut ctx) - if0, usd(10) as u64);
    assert_eq!(evs::<Liquidation>(&logs)[0].reward, usd(10) as u64);
    let ended = evs::<CrossLiquidationEnded>(&logs);
    assert_eq!((ended.len(), ended[0].totalCrossMaintMargin), (1, 0), "no cross position left");
    assert!(!flagged(&mut ctx, ALICE));
}

/// **Q3 — the cross fee is capped at `max(0, E_after)`.** The same close with $205 in the wallet:
/// `E_after = $5`, so the $10 fee due is cut to $5 and the wallet ends at exactly 0.
#[test]
fn a_cross_clearance_fee_is_capped_at_the_post_close_equity() {
    let (_g, mut ctx) = make_ctx();
    set_fee_rate(&mut ctx, M1, 125);
    long_10_on_m1(&mut ctx, usd(205));
    rest(&mut ctx, MAKER, M1, BUY, px(80), 10);
    let ledger = Ledger::open(&mut ctx);
    let if0 = insurance_fund(&mut ctx);

    let logs = tick(&mut ctx, M1, px(80));
    ledger.check(&mut ctx, "after the tick");
    assert_eq!(evs::<Liquidation>(&logs)[0].reward, usd(5) as u64);
    assert_eq!(wallet(&mut ctx, ALICE), 0);
    assert_eq!(insurance_fund(&mut ctx) - if0, usd(5) as u64);
}

/// **R5, solvent — the book-unfillable rest goes to ADL at the MARK** while the account has
/// equity: no bids at all, `E = $100 ≥ 0`, so BOB (the only opposite holder) takes the 10 at $80 —
/// economically neutral for him — and ALICE keeps her `E`.
#[test]
fn the_unfilled_rest_is_adld_at_the_mark_while_the_account_has_equity() {
    let (_g, mut ctx) = make_ctx();
    long_10_on_m1(&mut ctx, usd(300));
    let ledger = Ledger::open(&mut ctx);

    let logs = tick(&mut ctx, M1, px(80));
    ledger.check(&mut ctx, "after the tick");
    assert!(evs::<Trade>(&logs).is_empty(), "nothing in the book");
    assert_eq!(adls(&logs), vec![(ALICE, BOB, M1, 10, px(80))]);
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    assert_eq!(pos(&mut ctx, BOB, M1).amount, 0);
    assert_eq!(wallet(&mut ctx, ALICE), usd(100), "she keeps her equity");
    assert!(!flagged(&mut ctx, ALICE));
}

/// **R5, bankrupt — ADL at the MM-share bankruptcy price, and no fee.** `E = −$100` at $80: the
/// bankruptcy price is the one at which the close realizes `uPnL − E`, i.e. $90 — ABOVE the mark
/// for an insolvent long — so BOB, the winner, absorbs the $100 deficit (he books +$100 where the
/// mark would have given him +$200) and ALICE's wallet ends at exactly 0: not negative, nothing for
/// the insurance fund. The IOC's limit is that same $90 (band off), above every bid, so the book
/// leg fills nothing. The 1.25% fee is due on $900 of notional but capped at `E_after = 0`.
#[test]
fn a_bankrupt_account_is_adld_at_its_bankruptcy_price_and_pays_no_fee() {
    let (_g, mut ctx) = make_ctx();
    set_fee_rate(&mut ctx, M1, 125);
    long_10_on_m1(&mut ctx, usd(100));
    rest(&mut ctx, MAKER, M1, BUY, px(80), 10);
    let ledger = Ledger::open(&mut ctx);
    let (if0, bob_w0) = (insurance_fund(&mut ctx), wallet(&mut ctx, BOB));
    let bob_silo = pos(&mut ctx, BOB, M1).margin;

    let logs = tick(&mut ctx, M1, px(80));
    ledger.check(&mut ctx, "after the tick");
    let close: Vec<OrderPlaced> =
        evs::<OrderPlaced>(&logs).into_iter().filter(|o| o.user == ALICE).collect();
    assert_eq!(close[0].price, px(90));
    assert!(evs::<Trade>(&logs).is_empty(), "the $80 bids are under the limit");
    assert_eq!(adls(&logs), vec![(ALICE, BOB, M1, 10, px(90))]);
    assert_eq!(wallet(&mut ctx, ALICE), 0, "the deficit went to the winner, not below zero");
    assert_eq!(wallet(&mut ctx, BOB) - bob_w0, bob_silo + usd(100));
    assert_eq!(evs::<Liquidation>(&logs)[0].reward, 0);
    assert_eq!(insurance_fund(&mut ctx), if0);
    assert_eq!(evs::<CrossLiquidationStarted>(&logs)[0].crossMarginBalance, usd(-100));
    assert!(!flagged(&mut ctx, ALICE));
}

/// **Q4 — the IOC limit is the bankruptcy price CLAMPED into the band.** M1's band is 5%; at $80
/// the bankruptcy price ($70) is below the band's lower edge ($76), so the IOC goes out at $76: it
/// takes the 4 bid at $77 and stops there (the $75 bid is outside both the limit and the band),
/// which already restores the account — a partial liquidation.
#[test]
fn the_ioc_limit_is_clamped_into_the_price_band() {
    let (_g, mut ctx) = make_ctx();
    let mut mk = storage::load_market(&mut ctx, M1).unwrap().unwrap();
    mk.price_band_bps = 500;
    storage::save_market(&mut ctx, &mk).unwrap();
    long_10_on_m1(&mut ctx, usd(300));
    rest(&mut ctx, MAKER, M1, BUY, px(77), 4);
    rest(&mut ctx, MAKER, M1, BUY, px(75), 10);
    let ledger = Ledger::open(&mut ctx);

    let logs = tick(&mut ctx, M1, px(80));
    ledger.check(&mut ctx, "after the tick");
    let close: Vec<OrderPlaced> =
        evs::<OrderPlaced>(&logs).into_iter().filter(|o| o.user == ALICE).collect();
    assert_eq!(close[0].price, px(76), "clamped up from $70 to mark − 5%");
    let t = evs::<Trade>(&logs);
    assert_eq!((t.len(), t[0].quantity, t[0].price), (1, 4, px(77)));
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 6);
    assert!(!flagged(&mut ctx, ALICE));
}

// ── The episode: several markets ────────────────────────────────────────────────

/// ALICE: CROSSED 3x longs of 10 on M1 (against BOB) and M2 (against CAROL) at $100, the wallet `w`
/// left after both.
fn longs_on_m1_and_m2(ctx: &mut TestCtx, w: i64) {
    trade(ctx, M1, ALICE, BOB, BUY, px(100), 10);
    trade(ctx, M2, ALICE, CAROL, BUY, px(100), 10);
    set_wallet(ctx, ALICE, w);
}

/// **Q5 / D4 — a bankrupt account's deficit is split by maintenance margin.** M2's mark sits at
/// $90 (stored, no tick) and M1 ticks to $70: `E = $200 − $300 − $100 = −$200` against
/// `MM_1 = $116.67`, `MM_2 = $150`. The episode closes M1 first (the trigger), then M2, each by ADL
/// at its MM-share bankruptcy price: M1's share is `−$87.50` ($78.75, above its $70 mark), and with
/// the ratio `E / Σ MM` preserved by that close, M2's is `−$112.50` ($101.25, above its $90 mark).
/// The winners absorb the deficit in proportion to the markets' MM — BOB $87.50, CAROL $112.50 —
/// and ALICE's wallet ends at zero, not below.
#[test]
fn a_bankrupt_multi_market_account_splits_its_deficit_by_maintenance_margin() {
    let (_g, mut ctx) = make_ctx();
    longs_on_m1_and_m2(&mut ctx, usd(200));
    set_mark(&mut ctx, M2, px(90));
    let ledger = Ledger::open(&mut ctx);

    let logs = tick(&mut ctx, M1, px(70));
    ledger.check(&mut ctx, "after the tick");
    let liq: Vec<u64> = evs::<Liquidation>(&logs).iter().map(|l| l.marketId).collect();
    assert_eq!(liq, vec![M1, M2], "the trigger market first");
    let a = adls(&logs);
    assert_eq!(a.len(), 2);
    let near = |p: u64, want: u64| p.abs_diff(want) <= 1;
    assert_eq!((a[0].0, a[0].1, a[0].2, a[0].3), (ALICE, BOB, M1, 10));
    assert!(near(a[0].4, 7_875), "M1 at its MM-share price: {}", a[0].4);
    assert_eq!((a[1].0, a[1].1, a[1].2, a[1].3), (ALICE, CAROL, M2, 10));
    assert!(near(a[1].4, 10_125), "M2 at its MM-share price: {}", a[1].4);
    let w = wallet(&mut ctx, ALICE);
    assert!((0..usd(1)).contains(&w), "ends at zero (rounded in the account's favour): {w}");
    assert!(!flagged(&mut ctx, ALICE));
}

/// **Q10 — a STALE market is skipped, and its own tick resumes the episode.** The block clock is at
/// 10,000 and M2's mark was never published by its oracle (index timestamp 0 — far older than
/// `3 × 15 s`). M1's tick runs the episode: M1 closes, M2 is skipped, the account is still below
/// maintenance, so the flag STAYS — the account may only reduce (`-2023`) — and no `Ended` is
/// emitted. M2's own tick (fresh: its mark is this transaction's) finds the flagged account in its
/// registry and finishes the job.
#[test]
fn a_stale_market_is_skipped_and_its_own_tick_resumes_the_episode() {
    let (_g, mut ctx) = make_ctx();
    longs_on_m1_and_m2(&mut ctx, usd(200));
    set_mark(&mut ctx, M2, px(90));
    ctx.block.timestamp = U256::from(10_000);
    let ledger = Ledger::open(&mut ctx);

    let logs = tick_at(&mut ctx, M1, px(70), 10_000);
    ledger.check(&mut ctx, "after M1's tick");
    let liq: Vec<u64> = evs::<Liquidation>(&logs).iter().map(|l| l.marketId).collect();
    assert_eq!(liq, vec![M1], "M2's mark is stale: skipped");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    assert_eq!(pos(&mut ctx, ALICE, M2).amount, 10);
    assert!(evs::<CrossLiquidationEnded>(&logs).is_empty());
    assert!(flagged(&mut ctx, ALICE));
    assert!(fold(&mut ctx, ALICE).below_cross_maintenance());
    // In between: only reducing orders.
    rejects(
        &mut ctx,
        ALICE,
        &order(M3, BUY, px(100), 1, GTC),
        crate::trading::IN_LIQUIDATION_REJECT,
    );

    let logs = tick_at(&mut ctx, M2, px(90), 10_005);
    ledger.check(&mut ctx, "after M2's tick");
    let liq: Vec<u64> = evs::<Liquidation>(&logs).iter().map(|l| l.marketId).collect();
    assert_eq!(liq, vec![M2]);
    assert!(evs::<CrossLiquidationStarted>(&logs).is_empty(), "the same episode, resumed");
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
    assert_eq!(pos(&mut ctx, ALICE, M2).amount, 0);
    assert!(!flagged(&mut ctx, ALICE));
    let w = wallet(&mut ctx, ALICE);
    assert!((0..usd(1)).contains(&w), "{w}");
}

/// **Detection completeness and the close order.** ALICE holds a long of 20 on M1 (the larger MM)
/// and 5 on M2. A tick of M1 that leaves her healthy does nothing. M2's tick to $60 takes her ACCOUNT
/// under maintenance (`E = $350 < $383.33`) — it is caught on M2's tick, and M2, the trigger, closes
/// FIRST although M1's MM is larger: the 5 at $60 restore her (`E = $350 ≥ $333.33`), so M1 is never
/// touched.
#[test]
fn the_account_is_caught_on_the_tick_of_the_market_that_moved_which_closes_first() {
    let (_g, mut ctx) = make_ctx();
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 20);
    trade(&mut ctx, M2, ALICE, CAROL, BUY, px(100), 5);
    set_wallet(&mut ctx, ALICE, usd(550));
    rest(&mut ctx, MAKER, M2, BUY, px(60), 5);
    let ledger = Ledger::open(&mut ctx);

    let logs = tick(&mut ctx, M1, px(100));
    assert!(evs::<CrossLiquidationStarted>(&logs).is_empty(), "healthy: nothing");

    let logs = tick(&mut ctx, M2, px(60));
    ledger.check(&mut ctx, "after M2's tick");
    let started = evs::<CrossLiquidationStarted>(&logs);
    assert_eq!((started.len(), started[0].triggerMarketId), (1, M2));
    let liq: Vec<u64> = evs::<Liquidation>(&logs).iter().map(|l| l.marketId).collect();
    assert_eq!(liq, vec![M2]);
    assert_eq!(pos(&mut ctx, ALICE, M2).amount, 0);
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 20, "the stop condition held before M1");
    assert!(!flagged(&mut ctx, ALICE));
}

// ── Cancel-all, isolated markets included ──────────────────────────────────────

/// **Q2 / R3 — every open order of the account goes, in every market of its index:** a conditional
/// stop on the liquidated market, a book order on her ISOLATED market and one on a market where she
/// holds nothing — each with reason 3. Her ISOLATED position is left exactly as it was: the episode
/// closes CROSS positions only.
#[test]
fn a_mixed_account_has_every_order_cancelled_and_its_isolated_position_untouched() {
    let (_g, mut ctx) = make_ctx();
    set_type(&mut ctx, ALICE, M2, ISOLATED);
    set_leverage(&mut ctx, ALICE, M2, 1);
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    trade(&mut ctx, M2, ALICE, CAROL, SELL, px(100), 5);
    ok(
        &mut ctx,
        ALICE,
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: M1,
            side: SELL,
            orderType: 1,
            triggerPrice: px(50),
            price: 0,
            quantity: 10,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: RO,
        }
        .abi_encode(),
    );
    let stop = storage::load_user_algo_orders(&mut ctx, ALICE, M1).unwrap()[0];
    let iso_bid = rest(&mut ctx, ALICE, M2, BUY, px(90), 1);
    let m3_bid = rest(&mut ctx, ALICE, M3, BUY, px(50), 1);
    set_wallet(&mut ctx, ALICE, usd(300));
    let iso_before = pos(&mut ctx, ALICE, M2);
    let ledger = Ledger::open(&mut ctx);

    let logs = tick(&mut ctx, M1, px(80));
    ledger.check(&mut ctx, "after the tick");
    let mut cancels: Vec<([u8; 32], u64, u8)> = evs::<OrderCancelled>(&logs)
        .iter()
        .filter(|c| c.user == ALICE)
        .map(|c| (c.orderId.0, c.marketId, c.reason))
        .collect();
    cancels.sort();
    let mut want = vec![(iso_bid, M2, CANCEL_LIQUIDATION), (m3_bid, M3, CANCEL_LIQUIDATION)];
    want.sort();
    assert_eq!(cancels, want);
    let algo: Vec<([u8; 32], u8)> = evs::<AlgoOrderCancelled>(&logs)
        .iter()
        .filter(|c| c.user == ALICE)
        .map(|c| (c.algoId.0, c.reason))
        .collect();
    assert_eq!(algo, vec![(stop, CANCEL_LIQUIDATION)]);
    for m in MARKETS {
        assert_eq!(storage::book_order_count(&mut ctx, ALICE, m).unwrap(), 0);
        assert!(storage::load_user_algo_orders(&mut ctx, ALICE, m).unwrap().is_empty());
    }
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0, "the cross position closed (ADL at mark)");
    let iso = pos(&mut ctx, ALICE, M2);
    assert_eq!(
        (iso.amount, iso.v_quote_balance, iso.margin),
        (iso_before.amount, iso_before.v_quote_balance, iso_before.margin),
        "the isolated position is untouched"
    );
    assert!(!flagged(&mut ctx, ALICE));
}

// ── Budget and continuation ─────────────────────────────────────────────────────

/// **Out of budget, the episode records a CONTINUATION, and the drain resumes it.** ALICE is under
/// maintenance (marks stored, no tick) with orders in all three markets and cross longs on M1/M2.
/// An episode run with a budget of 2 cancels M1's and M2's orders and stops: the flag is set, M3's
/// order is still resting, nothing is closed, and the continuation is on M1 (the first market of the
/// close order still holding a cross position) — its `deferred_work` bit. In between the account
/// may only reduce. `drainDeferredWork(M1)` resumes the episode from M1's registry: M3's order is
/// cancelled, both markets close, the flag clears and the bit is cleared with nothing left.
#[test]
fn an_episode_out_of_budget_records_a_continuation_the_drain_resumes() {
    let (_g, mut ctx) = make_ctx();
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    trade(&mut ctx, M2, ALICE, CAROL, BUY, px(100), 10);
    for m in MARKETS {
        rest(&mut ctx, ALICE, m, BUY, px(50), 1);
    }
    set_wallet(&mut ctx, ALICE, usd(300));
    set_mark(&mut ctx, M1, px(80));
    let ledger = Ledger::open(&mut ctx);
    assert!(fold(&mut ctx, ALICE).below_cross_maintenance());

    let caller = EpisodeCaller { sweep_market: None, fresh_market: None, liquidator: KEEPER };
    let (mut budget, mut adl_budget, mut deferred) = (2, 128, false);
    let out = liquidate_cross_account(
        &mut ctx,
        ALICE,
        M1,
        caller,
        &mut budget,
        &mut adl_budget,
        &mut deferred,
    )
    .unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());
    ledger.check(&mut ctx, "after the budgeted episode");
    assert_eq!(out, CrossLiquidationOutcome::Liquidated { ended: false });
    assert_eq!((budget, deferred), (0, false));
    assert_eq!(evs::<CrossLiquidationStarted>(&logs).len(), 1);
    assert!(evs::<Liquidation>(&logs).is_empty());
    assert!(flagged(&mut ctx, ALICE));
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, M1).unwrap(), 0);
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, M2).unwrap(), 0);
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, M3).unwrap(), 1);
    let deferred_of = |ctx: &mut TestCtx, m: u64| {
        storage::load_market(ctx, m).unwrap().unwrap().deferred_work
    };
    assert_eq!(
        (deferred_of(&mut ctx, M1), deferred_of(&mut ctx, M2), deferred_of(&mut ctx, M3)),
        (1, 0, 0)
    );
    rejects(
        &mut ctx,
        ALICE,
        &order(M2, BUY, px(100), 1, GTC),
        crate::trading::IN_LIQUIDATION_REJECT,
    );

    let out = call(&mut ctx, KEEPER, &drainDeferredWorkCall { marketId: M1 }.abi_encode());
    assert!(!out.reverted, "{}", revert_reason(&out));
    let logs = JournalTr::take_logs(ctx.journal_mut());
    ledger.check(&mut ctx, "after the drain");
    assert!(!drainDeferredWorkCall::abi_decode_returns(&out.bytes).unwrap(), "nothing left");
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, M3).unwrap(), 0);
    let liq: Vec<u64> = evs::<Liquidation>(&logs).iter().map(|l| l.marketId).collect();
    assert!(liq.contains(&M1), "the drain closed M1: {liq:?}");
    assert!(!fold(&mut ctx, ALICE).below_cross_maintenance());
    assert!(!flagged(&mut ctx, ALICE));
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
    assert_eq!(deferred_of(&mut ctx, M1), 0);
}

// ── Manual liquidate() ──────────────────────────────────────────────────────────

/// `liquidate()` of a CROSSED position judges the ACCOUNT: healthy, refused write-clean; below
/// maintenance, the episode runs from the GIVEN market (M2 here, with the larger MM elsewhere) with
/// the caller as liquidator.
#[test]
fn liquidate_judges_the_account_and_starts_the_episode_at_the_given_market() {
    let (_g, mut ctx) = make_ctx();
    longs_on_m1_and_m2(&mut ctx, usd(400));
    rejects(
        &mut ctx,
        KEEPER,
        &liquidate(ALICE, M1),
        crate::risk::LIQUIDATE_CROSS_HEALTHY_REJECT,
    );
    set_mark(&mut ctx, M1, px(70));
    let ledger = Ledger::open(&mut ctx);

    let logs = ok(&mut ctx, KEEPER, &liquidate(ALICE, M2));
    ledger.check(&mut ctx, "after liquidate()");
    let started = evs::<CrossLiquidationStarted>(&logs);
    assert_eq!((started.len(), started[0].triggerMarketId), (1, M2));
    let liq = evs::<Liquidation>(&logs);
    assert_eq!((liq[0].marketId, liq[0].liquidator, liq[0].marginType), (M2, KEEPER, CROSSED));
    assert!(!flagged(&mut ctx, ALICE));
    assert!(!fold(&mut ctx, ALICE).below_cross_maintenance());
}

/// A FLAG that outlived the account's last cross position — an interrupted episode whose account
/// then closed the rest itself — is in no registry, so no sweep can clear it. `liquidate()` for the
/// account through ANY market clears it (`Ended`), a market where it holds nothing included; an
/// unflagged account with nothing there is still "no open position".
#[test]
fn liquidate_clears_a_flag_that_outlived_the_last_cross_position() {
    let (_g, mut ctx) = make_ctx();
    long_10_on_m1(&mut ctx, usd(300));
    storage::set_in_liquidation_for_test(&mut ctx, ALICE, true).unwrap();
    // She closes it herself with a reduce-only order (accepted while flagged).
    rest(&mut ctx, MAKER, M1, BUY, px(100), 10);
    ok(&mut ctx, ALICE, &order_flags(M1, SELL, px(100), 10, IOC, RO));
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    assert!(flagged(&mut ctx, ALICE), "nothing cleared it");
    rejects(
        &mut ctx,
        ALICE,
        &order(M1, BUY, px(100), 1, GTC),
        crate::trading::IN_LIQUIDATION_REJECT,
    );

    let logs = ok(&mut ctx, KEEPER, &liquidate(ALICE, M3));
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
    assert!(evs::<Liquidation>(&logs).is_empty());
    assert!(!flagged(&mut ctx, ALICE));
    rejects(&mut ctx, KEEPER, &liquidate(ALICE, M3), "liquidate: no open position");
    ok(&mut ctx, ALICE, &order(M1, BUY, px(90), 1, GTC));
}

// ── J4: total ───────────────────────────────────────────────────────────────────

/// **J4 — the episode is TOTAL.** ALICE holds cross longs on all three markets; M3's mark is 0 (its
/// leg counts `uPnL = MM = 0`, nothing to close at) and M2's pending funding is past `i64` (the fold
/// leaves that leg out). M1's tick takes the account under maintenance: the sweep closes M1 and
/// skips both — no `Err`, no panic — and the skipped legs are exactly as they were.
#[test]
fn the_episode_skips_a_zero_mark_and_an_unpriceable_leg_without_failing() {
    let (_g, mut ctx) = make_ctx();
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    trade(&mut ctx, M2, ALICE, CAROL, BUY, px(100), 1);
    trade(&mut ctx, M3, ALICE, CAROL, BUY, px(100), 1);
    set_wallet(&mut ctx, ALICE, usd(200));
    set_mark(&mut ctx, M3, 0);
    let fs = FundingState {
        last_funding_rate: 0,
        next_funding_ts: 0,
        cumulative_funding_index: 1_000_000_000_000_000_000_000,
    };
    storage::save_funding_state(&mut ctx, M2, &fs).unwrap();
    let (m2_before, m3_before) = (pos(&mut ctx, ALICE, M2), pos(&mut ctx, ALICE, M3));
    let f = fold(&mut ctx, ALICE);
    assert_eq!(f.faulted_legs, 1, "M2's leg cannot be priced");

    let logs = tick(&mut ctx, M1, px(70));
    let liq: Vec<u64> = evs::<Liquidation>(&logs).iter().map(|l| l.marketId).collect();
    assert_eq!(liq, vec![M1]);
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    assert_eq!(pos(&mut ctx, ALICE, M2), m2_before);
    assert_eq!(pos(&mut ctx, ALICE, M3), m3_before);
}

// ── A randomised multi-market run ──────────────────────────────────────────────

/// A deterministic PRNG (xorshift64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// **The sweep never panics, custody always closes, and detection is complete** across a
/// randomised run on three markets: four traders — ALICE and BOB CROSSED everywhere, CAROL CROSSED
/// on M1 / ISOLATED elsewhere, KEEPER ISOLATED — on thin wallets at random leverage, trading against
/// each other and a MAKER, with the clearance fee on, while the oracle walks every market up and down
/// by up to 25% a tick. After EVERY step the custody identity holds and `Σ amount == 0` per market;
/// after every tick of `m`, no account holding a cross position in `m` is left below maintenance
/// unless it is still flagged (the episode could not finish). Coverage floors: cross episodes,
/// bankrupt ones, and multi-market closes are reached.
#[test]
fn a_randomised_multi_market_run_never_panics_and_detection_is_complete() {
    let (_g, mut ctx) = make_ctx();
    for m in MARKETS {
        set_fee_rate(&mut ctx, m, 50);
        // A live 10% band, so a liquidation closes near the mark, as on a real market.
        let mut mk = storage::load_market(&mut ctx, m).unwrap().unwrap();
        mk.price_band_bps = 1_000;
        storage::save_market(&mut ctx, &mk).unwrap();
    }
    // A deep fund: an ISOLATED liquidation that closes past its bankruptcy is absorbed rather than
    // left unbacked (an unbacked deficit is the one way the identity can open, and it would be an
    // isolated-path event: `InsuranceFundDepleted` is asserted absent below).
    storage::save_insurance_fund(&mut ctx, usd(1_000_000_000) as u64).unwrap();
    set_type(&mut ctx, CAROL, M1, CROSSED);
    let traders = [ALICE, BOB, CAROL, KEEPER];
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, CROSSED);
    }
    let mut rng = Rng(0x0c20_5b1d_9e3f_000f);
    for &u in &traders {
        set_wallet(&mut ctx, u, usd(1_000));
        for m in MARKETS {
            set_leverage(&mut ctx, u, m, 1 + rng.below(3));
        }
    }
    let mut ledger = Ledger::open(&mut ctx);
    let mut index = [px(100); 3];
    let (mut episodes, mut bankrupt, mut multi, mut ticks) = (0u32, 0u32, 0u32, 0u32);
    for step in 0..1_500u32 {
        let m_i = rng.below(3) as usize;
        let m = MARKETS[m_i];
        match rng.below(10) {
            0..=5 => {
                // A trade: the taker crosses the MAKER's fresh quote at the mark ± a little.
                // Sized off the taker's available balance at its leverage, so accounts run levered.
                let u = traders[rng.below(4) as usize];
                let side = (rng.below(2)) as u8;
                let mark = storage::load_mark_price(&mut ctx, m).unwrap();
                let lev = pos(&mut ctx, u, m).leverage.max(1) as u128;
                let a = fold(&mut ctx, u).a.max(0) as u128;
                let affordable = (a * lev / (mark as u128 * 10_000)).min(200) as u64;
                let qty = 1 + rng.below(affordable.max(1));
                let p = if side == BUY {
                    mark + rng.below(mark / 50 + 1)
                } else {
                    mark.saturating_sub(rng.below(mark / 50 + 1)).max(1)
                };
                ok(&mut ctx, MAKER, &order(m, 1 - side, p, qty, GTC));
                // Refusals (margin, `-2023`) are part of the run.
                let _ = call(&mut ctx, u, &order(m, side, p, qty, IOC));
                let _ = JournalTr::take_logs(ctx.journal_mut());
                // Whatever the taker did not take is withdrawn again.
                let ids: Vec<[u8; 32]> = storage::load_buy_orders(&mut ctx, MAKER, m)
                    .unwrap()
                    .iter()
                    .chain(storage::load_sell_orders(&mut ctx, MAKER, m).unwrap().iter())
                    .map(|e| e.order_id)
                    .collect();
                for id in ids {
                    ok(
                        &mut ctx,
                        MAKER,
                        &crate::interface::IPerpDex::cancelOrderCall {
                            orderId: id.into(),
                            marketId: m,
                        }
                        .abi_encode(),
                    );
                }
            }
            6 => {
                // A resting quote of a trader, sometimes left for the episode to cancel.
                let u = traders[rng.below(4) as usize];
                let mark = storage::load_mark_price(&mut ctx, m).unwrap();
                let side = rng.below(2) as u8;
                let p = if side == BUY { mark * 9 / 10 } else { mark * 11 / 10 };
                let _ = call(&mut ctx, u, &order(m, side, p.max(1), 1 + rng.below(3), GTC));
                let _ = JournalTr::take_logs(ctx.journal_mut());
            }
            _ => {
                // The oracle: up or down by up to 25%, floored at $5.
                let holders: Vec<Address> = traders
                    .iter()
                    .copied()
                    .filter(|&u| {
                        let p = pos(&mut ctx, u, m);
                        p.amount != 0
                            && crate::margin_type::effective_margin_type(&p) == MarginType::Crossed
                    })
                    .collect();
                let d = index[m_i] * rng.below(31) / 100;
                index[m_i] =
                    if rng.below(2) == 0 { index[m_i] + d } else { index[m_i].saturating_sub(d) }
                        .max(px(5));
                let logs = tick(&mut ctx, m, index[m_i]);
                assert!(
                    evs::<crate::interface::IPerpDex::InsuranceFundDepleted>(&logs).is_empty(),
                    "step {step}: the fund ran dry"
                );
                ticks += 1;
                let started = evs::<CrossLiquidationStarted>(&logs);
                episodes += started.len() as u32;
                bankrupt += started.iter().filter(|s| s.crossMarginBalance < 0).count() as u32;
                for s in &started {
                    let n = evs::<Liquidation>(&logs).iter().filter(|l| l.user == s.user).count();
                    multi += (n > 1) as u32;
                }
                for u in holders {
                    let f = fold(&mut ctx, u);
                    assert!(
                        !f.below_cross_maintenance() || flagged(&mut ctx, u),
                        "step {step}: {u} holds a cross position in market {m} and was left \
                         below maintenance, unflagged, by its tick: {f:?}"
                    );
                }
            }
        }
        ledger.check(&mut ctx, &format!("step {step}"));
        // Top up a FLAT account the run has drained, so it keeps trading.
        for &u in &traders {
            let flat = storage::load_account(&mut ctx, u).unwrap().open_position_count == 0;
            if flat && wallet(&mut ctx, u) < usd(200) && !flagged(&mut ctx, u) {
                let a = storage::load_account(&mut ctx, u).unwrap();
                let spot: U256 = a.usdc_balance.clone().into();
                let top_up = usd(1_000);
                storage::mutate_account(&mut ctx, u, |a| {
                    a.usdc_balance = (spot + U256::from(top_up as u64)).into();
                })
                .unwrap();
                // New money from outside the system: the identity's total grows by exactly it.
                ledger.total += top_up as i128;
                ok(
                    &mut ctx,
                    u,
                    &crate::interface::IPerpDex::transferToPerpCall { amount: top_up as u64 }
                        .abi_encode(),
                );
            }
        }
    }
    eprintln!(
        "coverage: ticks {ticks}, cross episodes {episodes}, bankrupt {bankrupt}, multi-market \
         {multi}"
    );
    // Floors about half of what this seed reaches (26 / 4 / 3); 120 seeds of this run were checked
    // free of any failure but these floors.
    assert!(episodes >= 13, "cross episodes {episodes}");
    assert!(bankrupt >= 2, "bankrupt episodes {bankrupt}");
    assert!(multi >= 1, "multi-market episodes {multi}");
}
