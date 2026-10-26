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
//! * budget and continuation (`deferred_work`, resumed by the drain — or by the tick of ANY market
//!   the account holds), the `-2023` freeze in between (reduce-only orders accepted and filled, R1;
//!   everything else refused or cancelled), a deposit curing the account (the next tick ends the
//!   episode with one write), a market whose oracle stopped (skipped until it publishes again);
//! * the flag never outlives the last cross position, whatever route closes it (a reduce-only
//!   close, a maker fill, a fired stop, ADL against the flagged account);
//! * manual `liquidate()`: healthy refused write-clean, a recovered flag cleared through any
//!   market, the start market;
//! * cross ADL (R5): a losing cross winner ranks `uPnL · E / N²` among isolated winners on their
//!   ROE, the clean tier is judged per fill (`E − take · |fill − mark| ≥ 0`), and a fill the
//!   account cannot carry re-queues it behind the clean tier;
//! * negative-balance clearance (Q8, R4): a negative wallet is cleared by the insurance fund at the
//!   end of the call that closed the account's LAST position — by a user close, a maker fill, a
//!   fired stop, a cross episode (sweep or drain), an ADL fill on the winning side — never while a
//!   position (an isolated silo included) is open, and with a short fund what it cannot pay is
//!   socialised (`InsuranceFundDepleted`);
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
        setMarginTypeCall, transferFromPerpCall, transferToPerpCall, updateIndexPriceCall,
        AccountBalanceChanged, Adl,
        AlgoOrderCancelled, CrossLiquidationEnded, CrossLiquidationStarted, InsuranceFundChanged,
        InsuranceFundDepleted, Liquidation, OrderCancelled, OrderPlaced, Trade,
    },
    risk::cross_liquidation::{liquidate_cross_account, CrossLiquidationOutcome, EpisodeCaller},
    run_perp_dex_call, storage,
    types::{
        AccountUpdateReason, FundingState, IndexPriceState, MarginTiers, MarginType, Market,
        UserAccount,
    },
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

/// Cross margin as it ships (the real gate); every account but ALICE switched to ISOLATED
/// everywhere, so ALICE — at the default — is the only CROSSED account unless a test says
/// otherwise.
fn make_ctx() -> TestCtx {
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
    ctx
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

/// [`rest`] with order flags (`RO`).
fn rest_flags(ctx: &mut TestCtx, user: Address, m: u64, side: u8, price: u64, qty: u64, flags: u8) {
    let logs = ok(ctx, user, &order_flags(m, side, price, qty, GTC, flags));
    assert!(evs::<Trade>(&logs).is_empty(), "the resting order traded");
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

/// Fixture only: sets the insurance fund to `amount` — new money for the identity, so BEFORE
/// [`Ledger::open`]. Returns the fund.
fn insurance_fund_seeded(ctx: &mut TestCtx, amount: i64) -> u64 {
    storage::save_insurance_fund(ctx, amount as u64).unwrap();
    amount as u64
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
    let mut ctx = make_ctx();
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
    let mut ctx = make_ctx();
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
    let mut ctx = make_ctx();
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
    let mut ctx = make_ctx();
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
    let mut ctx = make_ctx();
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
    let mut ctx = make_ctx();
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
    let mut ctx = make_ctx();
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
    let mut ctx = make_ctx();
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
    let mut ctx = make_ctx();
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
    let mut ctx = make_ctx();
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
    let mut ctx = make_ctx();
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

/// `deferred_work` of market `m`.
fn deferred_of(ctx: &mut TestCtx, m: u64) -> u8 {
    storage::load_market(ctx, m).unwrap().unwrap().deferred_work
}

/// An episode run directly with `budget` steps and no sweep owning it (a continuation is written to
/// its market's `deferred_work`), its logs drained.
fn budgeted_episode(
    ctx: &mut TestCtx,
    user: Address,
    start: u64,
    budget: u32,
) -> CrossLiquidationOutcome {
    let caller = EpisodeCaller { sweep_market: None, fresh_market: None, liquidator: KEEPER };
    let (mut budget, mut adl_budget, mut deferred) = (budget, 128, false);
    let out = liquidate_cross_account(
        ctx,
        user,
        start,
        caller,
        &mut budget,
        &mut adl_budget,
        &mut deferred,
    )
    .unwrap();
    storage::flush_account_snapshots(ctx).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());
    out
}

/// New money from outside the system into `user`'s perp wallet, through `transferToPerp`; the
/// identity's total grows by exactly it.
fn deposit(ctx: &mut TestCtx, ledger: &mut Ledger, user: Address, amount: i64) -> Vec<Log> {
    let a = storage::load_account(ctx, user).unwrap();
    let spot: U256 = a.usdc_balance.clone().into();
    storage::mutate_account(ctx, user, |a| {
        a.usdc_balance = (spot + U256::from(amount as u64)).into();
    })
    .unwrap();
    ledger.total += amount as i128;
    ok(ctx, user, &transferToPerpCall { amount: amount as u64 }.abi_encode())
}

/// **A continuation is resumed by the tick of ANY market the account holds.** ALICE (cross longs on
/// M1 and M2, an order in each of M1–M3) is caught below maintenance and her episode runs out of
/// budget after two cancels: flagged, M3's order still resting, nothing closed, the continuation on
/// M1. It is M2's tick — not M1's — that comes next: its sweep finds the flagged account in M2's
/// registry and resumes the SAME episode from M2 (no second `Started`): M3's order goes, M2 (the
/// market that ticked) closes first, then M1, and the episode ends. M1's bit is still set; M1's
/// drain finds nothing left and clears it.
#[test]
fn an_interrupted_episode_resumes_on_the_tick_of_another_market() {
    let mut ctx = make_ctx();
    longs_on_m1_and_m2(&mut ctx, WALLET);
    for m in MARKETS {
        rest(&mut ctx, ALICE, m, BUY, px(50), 1);
    }
    set_wallet(&mut ctx, ALICE, usd(300));
    set_mark(&mut ctx, M1, px(80));
    let ledger = Ledger::open(&mut ctx);
    assert!(fold(&mut ctx, ALICE).below_cross_maintenance());

    let out = budgeted_episode(&mut ctx, ALICE, M1, 2);
    ledger.check(&mut ctx, "after the budgeted episode");
    assert_eq!(out, CrossLiquidationOutcome::Liquidated { ended: false });
    assert!(flagged(&mut ctx, ALICE));
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, M3).unwrap(), 1);
    assert_eq!((deferred_of(&mut ctx, M1), deferred_of(&mut ctx, M2)), (1, 0));

    let logs = tick(&mut ctx, M2, px(100));
    ledger.check(&mut ctx, "after M2's tick");
    assert!(evs::<CrossLiquidationStarted>(&logs).is_empty(), "the same episode, resumed");
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, M3).unwrap(), 0);
    let liq: Vec<u64> = evs::<Liquidation>(&logs).iter().map(|l| l.marketId).collect();
    assert_eq!(liq, vec![M2, M1], "the market that ticked first");
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
    assert!(!flagged(&mut ctx, ALICE));
    assert_eq!(deferred_of(&mut ctx, M1), 1, "M2's sweep does not own M1's bit");

    let out = call(&mut ctx, KEEPER, &drainDeferredWorkCall { marketId: M1 }.abi_encode());
    assert!(!drainDeferredWorkCall::abi_decode_returns(&out.bytes).unwrap(), "nothing left");
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert!(evs::<Liquidation>(&logs).is_empty());
    assert_eq!(deferred_of(&mut ctx, M1), 0);
}

/// **A deposit cures a flagged account; the next tick ends the episode with one write.** ALICE's
/// episode runs out of budget on its one cancel (M3), flagged with nothing closed. In between she
/// is in liquidation mode — an opening order is `-2023`, so is a margin-type switch, while a
/// reduce-only order is accepted and rests — and she deposits enough to be back above maintenance.
/// The flag stays until a tick looks: M2's tick finds her healthy and ENDS the episode (`Ended`,
/// nothing closed, nothing cancelled — her reduce-only order keeps resting), and she may open
/// again.
#[test]
fn a_deposit_cures_a_flagged_account_and_the_next_tick_ends_the_episode() {
    let mut ctx = make_ctx();
    longs_on_m1_and_m2(&mut ctx, WALLET);
    rest(&mut ctx, ALICE, M3, BUY, px(50), 1);
    set_wallet(&mut ctx, ALICE, usd(300));
    set_mark(&mut ctx, M1, px(80));
    let mut ledger = Ledger::open(&mut ctx);

    assert_eq!(
        budgeted_episode(&mut ctx, ALICE, M1, 1),
        CrossLiquidationOutcome::Liquidated { ended: false }
    );
    ledger.check(&mut ctx, "after the budgeted episode");
    assert!(flagged(&mut ctx, ALICE));
    assert_eq!((pos(&mut ctx, ALICE, M1).amount, pos(&mut ctx, ALICE, M2).amount), (10, 10));
    assert_eq!(deferred_of(&mut ctx, M1), 1);

    // Liquidation mode.
    let open = order(M2, BUY, px(90), 1, GTC);
    rejects(&mut ctx, ALICE, &open, crate::trading::IN_LIQUIDATION_REJECT);
    rejects(
        &mut ctx,
        ALICE,
        &setMarginTypeCall { marketId: M3, marginType: ISOLATED }.abi_encode(),
        "setMarginType: -2023 User in liquidation mode now.",
    );
    rest_flags(&mut ctx, ALICE, M2, SELL, px(120), 5, RO);

    // The cure: $1,000 in, E = $1,100 ≥ Σ MM = $300 (and above initial margin, A = $500).
    deposit(&mut ctx, &mut ledger, ALICE, usd(1_000));
    ledger.check(&mut ctx, "after the deposit");
    assert!(!fold(&mut ctx, ALICE).below_cross_maintenance());
    assert!(flagged(&mut ctx, ALICE), "nothing has looked yet");
    rejects(&mut ctx, ALICE, &open, crate::trading::IN_LIQUIDATION_REJECT);

    let logs = tick(&mut ctx, M2, px(100));
    ledger.check(&mut ctx, "after M2's tick");
    assert!(evs::<Liquidation>(&logs).is_empty(), "nothing closed");
    assert!(evs::<OrderCancelled>(&logs).is_empty(), "nothing cancelled");
    let ended = evs::<CrossLiquidationEnded>(&logs);
    assert_eq!((ended.len(), ended[0].marketId), (1, M2));
    assert!(!flagged(&mut ctx, ALICE));
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, M2).unwrap(), 1, "the RO order rests on");
    ok(&mut ctx, ALICE, &order(M2, BUY, px(90), 1, GTC));
}

/// **R1 while flagged: her reduce-only order fills, everything else of hers is frozen.** ALICE
/// (cross long 10 on M1, orders on M2 and M3) is flagged with the continuation on M1 after one
/// cancel. She rests a reduce-only sell of 5 at $80 (Q11 accepts it) and a taker lifts it — a
/// flagged account's reduce-only maker order FILLS (R1) — which takes her back above maintenance.
/// Her opening bid on M3, reached by a seller, is CANCELLED instead of filled (reason 3). M1's
/// drain then finds her healthy and ends the episode with one write.
#[test]
fn a_reduce_only_fill_while_flagged_restores_the_account_and_the_drain_ends_it() {
    let mut ctx = make_ctx();
    long_10_on_m1(&mut ctx, WALLET);
    rest(&mut ctx, ALICE, M2, BUY, px(50), 1);
    let m3_bid = rest(&mut ctx, ALICE, M3, BUY, px(50), 1);
    set_wallet(&mut ctx, ALICE, usd(300));
    set_mark(&mut ctx, M1, px(80));
    let ledger = Ledger::open(&mut ctx);

    budgeted_episode(&mut ctx, ALICE, M1, 1);
    ledger.check(&mut ctx, "after the budgeted episode");
    assert!(flagged(&mut ctx, ALICE));
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, M3).unwrap(), 1);
    assert_eq!(deferred_of(&mut ctx, M1), 1);

    rest_flags(&mut ctx, ALICE, M1, SELL, px(80), 5, RO);
    let logs = ok(&mut ctx, BOB, &order(M1, BUY, px(80), 5, IOC));
    ledger.check(&mut ctx, "after the reduce-only fill");
    let t = evs::<Trade>(&logs);
    assert_eq!((t.len(), t[0].maker, t[0].quantity), (1, ALICE, 5), "R1: it filled");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 5);
    assert!(!fold(&mut ctx, ALICE).below_cross_maintenance(), "E = $100 ≥ Σ MM = $66.67");
    assert!(flagged(&mut ctx, ALICE));

    let logs = ok(&mut ctx, CAROL, &order(M3, SELL, px(50), 1, IOC));
    ledger.check(&mut ctx, "after the frozen bid was reached");
    assert!(evs::<Trade>(&logs).is_empty(), "a flagged account's opening order does not fill");
    let c = evs::<OrderCancelled>(&logs);
    assert_eq!((c.len(), c[0].orderId.0, c[0].reason), (1, m3_bid, CANCEL_LIQUIDATION));

    let out = call(&mut ctx, KEEPER, &drainDeferredWorkCall { marketId: M1 }.abi_encode());
    assert!(!out.reverted);
    let logs = JournalTr::take_logs(ctx.journal_mut());
    ledger.check(&mut ctx, "after the drain");
    assert!(evs::<Liquidation>(&logs).is_empty());
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
    assert!(!flagged(&mut ctx, ALICE));
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 5, "the rest is hers");
    assert_eq!(deferred_of(&mut ctx, M1), 0);
}

/// **A market whose oracle STOPPED.** As in the stale-skip test, M1's tick closes M1 and skips M2
/// (its mark is older than `3 × 15 s`): flagged, below maintenance, and NO continuation recorded —
/// a same-block retry could not close at a price the oracle no longer publishes. A keeper's
/// `liquidate(ALICE, M2)` meanwhile is no way around the guard: it skips M2 too, writes nothing and
/// fails nothing. Once M2's oracle publishes again (its index timestamp fresh — here the fixture
/// stands in for a tick whose own sweep did not reach her), the same `liquidate()` closes M2 and
/// ends the episode.
#[test]
fn a_stopped_oracle_holds_the_market_until_it_publishes_again() {
    let mut ctx = make_ctx();
    longs_on_m1_and_m2(&mut ctx, usd(200));
    set_mark(&mut ctx, M2, px(90));
    ctx.block.timestamp = U256::from(10_000);
    let ledger = Ledger::open(&mut ctx);

    let logs = tick_at(&mut ctx, M1, px(70), 10_000);
    ledger.check(&mut ctx, "after M1's tick");
    let liq: Vec<u64> = evs::<Liquidation>(&logs).iter().map(|l| l.marketId).collect();
    assert_eq!(liq, vec![M1]);
    assert!(flagged(&mut ctx, ALICE));
    assert_eq!((deferred_of(&mut ctx, M1), deferred_of(&mut ctx, M2)), (0, 0), "no continuation");

    let writes = ctx.perp_write_count();
    let logs = ok(&mut ctx, KEEPER, &liquidate(ALICE, M2));
    assert!(logs.is_empty(), "skipped: nothing to log");
    assert_eq!(ctx.perp_write_count(), writes, "and nothing written");
    assert_eq!(pos(&mut ctx, ALICE, M2).amount, 10);
    assert!(flagged(&mut ctx, ALICE));

    storage::save_index_price_state(
        &mut ctx,
        M2,
        &IndexPriceState { index_price: px(90), timestamp: 10_000 },
    )
    .unwrap();
    let logs = ok(&mut ctx, KEEPER, &liquidate(ALICE, M2));
    ledger.check(&mut ctx, "after liquidate()");
    let liq = evs::<Liquidation>(&logs);
    assert_eq!((liq.len(), liq[0].marketId, liq[0].liquidator), (1, M2, KEEPER));
    assert_eq!(pos(&mut ctx, ALICE, M2).amount, 0);
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
    assert!(!flagged(&mut ctx, ALICE));
}

// ── Manual liquidate() ──────────────────────────────────────────────────────────

/// `liquidate()` of a CROSSED position judges the ACCOUNT: healthy, refused write-clean; below
/// maintenance, the episode runs from the GIVEN market (M2 here, with the larger MM elsewhere) with
/// the caller as liquidator.
#[test]
fn liquidate_judges_the_account_and_starts_the_episode_at_the_given_market() {
    let mut ctx = make_ctx();
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

/// `liquidate()` for a FLAGGED account goes to the episode through ANY market — one where it holds
/// nothing included — so a keeper need not know where the account trades: a flag on an account
/// that has recovered (here a fixture flag on a flat account, the one way to get a flag without a
/// cross position now) is ended (`Ended`, no `Liquidation`); an unflagged account with nothing
/// there is still "no open position".
#[test]
fn liquidate_ends_a_recovered_flag_through_any_market() {
    let mut ctx = make_ctx();
    storage::set_in_liquidation_for_test(&mut ctx, ALICE, true).unwrap();
    rejects(
        &mut ctx,
        ALICE,
        &order(M1, BUY, px(100), 1, GTC),
        crate::trading::IN_LIQUIDATION_REJECT,
    );

    let logs = ok(&mut ctx, KEEPER, &liquidate(ALICE, M3));
    let ended = evs::<CrossLiquidationEnded>(&logs);
    assert_eq!((ended.len(), ended[0].marketId), (1, M3));
    assert!(evs::<Liquidation>(&logs).is_empty());
    assert!(!flagged(&mut ctx, ALICE));
    rejects(&mut ctx, KEEPER, &liquidate(ALICE, M3), "liquidate: no open position");
    ok(&mut ctx, ALICE, &order(M1, BUY, px(90), 1, GTC));
}

// ── The flag never outlives the last cross position ─────────────────────────────

/// **The flag ends with the account's last cross position, whatever closed it.** ALICE is flagged
/// (an episode left unfinished — the fixture flag stands in for it) and holds cross longs on M1 and
/// M2 plus an ISOLATED long on M3. Her own reduce-only close of M1 (Q11) leaves the flag: M2 is
/// still a cross position, and its ticks will resume the episode. Her reduce-only RESTING order on
/// M2, filled as a maker (R1), closes the LAST cross position — and the end of that call ends the
/// episode (`Ended` at M2, `Σ MM` 0) although her isolated position is still open: the flag is
/// about cross exposure. She may open again at once.
#[test]
fn the_flag_ends_with_the_last_cross_position_closed_by_any_route() {
    let mut ctx = make_ctx();
    set_type(&mut ctx, ALICE, M3, ISOLATED);
    set_leverage(&mut ctx, ALICE, M3, 1);
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    trade(&mut ctx, M2, ALICE, CAROL, BUY, px(100), 10);
    trade(&mut ctx, M3, ALICE, CAROL, BUY, px(100), 1);
    storage::set_in_liquidation_for_test(&mut ctx, ALICE, true).unwrap();
    let ledger = Ledger::open(&mut ctx);

    rest(&mut ctx, MAKER, M1, BUY, px(100), 10);
    let logs = ok(&mut ctx, ALICE, &order_flags(M1, SELL, px(100), 10, IOC, RO));
    ledger.check(&mut ctx, "after the M1 close");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    assert!(evs::<CrossLiquidationEnded>(&logs).is_empty(), "M2 is still a cross position");
    assert!(flagged(&mut ctx, ALICE));

    rest_flags(&mut ctx, ALICE, M2, SELL, px(100), 10, RO);
    let logs = ok(&mut ctx, BOB, &order(M2, BUY, px(100), 10, IOC));
    ledger.check(&mut ctx, "after the M2 maker close");
    assert_eq!(pos(&mut ctx, ALICE, M2).amount, 0);
    assert_eq!(pos(&mut ctx, ALICE, M3).amount, 1, "the isolated position is untouched");
    let ended = evs::<CrossLiquidationEnded>(&logs);
    assert_eq!(ended.len(), 1);
    assert_eq!((ended[0].user, ended[0].marketId, ended[0].totalCrossMaintMargin), (ALICE, M2, 0));
    assert_eq!(ended[0].crossMarginBalance, wallet(&mut ctx, ALICE), "E is the wallet now");
    assert!(!flagged(&mut ctx, ALICE));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(90), 1, GTC));
}

/// The same for a last cross position closed by a FIRED STOP (on M1's tick, before the sweep) and
/// for one closed by ADL with the flagged account on the WINNING side (CAROL's isolated long
/// liquidated by `liquidate()` against ALICE's cross short): each ends the episode in its own call.
#[test]
fn a_fired_stop_or_an_adl_fill_closing_the_last_cross_position_ends_the_episode() {
    // A fired stop.
    let mut ctx = make_ctx();
    long_10_on_m1(&mut ctx, usd(400));
    ok(
        &mut ctx,
        ALICE,
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: M1,
            side: SELL,
            orderType: 1,
            triggerPrice: px(95),
            price: 0,
            quantity: 10,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: RO,
        }
        .abi_encode(),
    );
    storage::set_in_liquidation_for_test(&mut ctx, ALICE, true).unwrap();
    rest(&mut ctx, MAKER, M1, BUY, px(90), 10);
    let logs = tick(&mut ctx, M1, px(90));
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
    assert!(!flagged(&mut ctx, ALICE));
    drop(ctx);

    // An ADL fill against the flagged account.
    let mut ctx = make_ctx();
    set_leverage(&mut ctx, CAROL, M1, 3);
    trade(&mut ctx, M1, CAROL, ALICE, BUY, px(100), 4);
    storage::set_in_liquidation_for_test(&mut ctx, ALICE, true).unwrap();
    set_mark(&mut ctx, M1, px(60));
    let ledger = Ledger::open(&mut ctx);
    let logs = ok(&mut ctx, KEEPER, &liquidate(CAROL, M1));
    ledger.check(&mut ctx, "after the ADL");
    assert_eq!(adls(&logs).iter().map(|a| a.1).collect::<Vec<_>>(), vec![ALICE]);
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    let ended = evs::<CrossLiquidationEnded>(&logs);
    assert_eq!((ended.len(), ended[0].user, ended[0].marketId), (1, ALICE, M1));
    assert!(!flagged(&mut ctx, ALICE));
}

// ── Cross ADL: ranking and the per-fill clean tier (R5) ─────────────────────────

/// `maker` rests `qty` at `price` on `m` with the mark moved there first (fixture), `taker` takes
/// it.
fn trade_at(
    ctx: &mut TestCtx,
    m: u64,
    taker: Address,
    maker: Address,
    side: u8,
    price: u64,
    qty: u64,
) {
    set_mark(ctx, m, price);
    trade(ctx, m, taker, maker, side, price, qty);
}

/// **R5 — a CROSSED winner with a LOSS ranks `uPnL · E / N²` (Binance's `PnL% / EffLev`), mixed
/// with an isolated winner on its ROE.** CAROL's ISOLATED 3x long of 5 (entry $100) is liquidated
/// by `liquidate()` at a $60 mark on an empty book: insolvent, so ADL fills at her bankruptcy price
/// ≈ $66.67. The shorts: KEEPER (ISOLATED, entered at $100, +$40) and ALICE and BOB (CROSSED, short
/// 2 each, entered at $50 — both at −$20). KEEPER's positive rank goes first. Among the two losing
/// cross accounts the one with the SMALLER equity goes next — ALICE, `E = $80`, rank
/// `−20 · 80 / 120²` against BOB's `−20 · 9,980 / 120²` — where `uPnL / E` would have picked BOB
/// (`−20 / 9,980` is the larger). Both are clean for their 2 at $6.67 a unit over the mark.
#[test]
fn adl_ranks_a_losing_cross_winner_by_upnl_times_e_over_n_squared() {
    let mut ctx = make_ctx();
    set_type(&mut ctx, BOB, M1, CROSSED);
    set_leverage(&mut ctx, BOB, M1, 3);
    set_leverage(&mut ctx, CAROL, M1, 3);
    set_leverage(&mut ctx, KEEPER, M1, 1);
    trade(&mut ctx, M1, CAROL, MAKER, BUY, px(100), 4);
    trade(&mut ctx, M1, CAROL, KEEPER, BUY, px(100), 1);
    // MAKER buys its short back from ALICE and BOB at $50: they are short 2 each from there.
    trade_at(&mut ctx, M1, MAKER, ALICE, BUY, px(50), 2);
    trade_at(&mut ctx, M1, MAKER, BOB, BUY, px(50), 2);
    assert_eq!(pos(&mut ctx, MAKER, M1).amount, 0);
    set_wallet(&mut ctx, ALICE, usd(100));
    set_wallet(&mut ctx, BOB, usd(10_000));
    set_mark(&mut ctx, M1, px(60));
    let ledger = Ledger::open(&mut ctx);
    assert_eq!(
        (fold(&mut ctx, ALICE).e, fold(&mut ctx, BOB).e),
        (usd(80) as i128, usd(9_980) as i128)
    );

    let logs = ok(&mut ctx, ADMIN, &liquidate(CAROL, M1));
    ledger.check(&mut ctx, "after the liquidation + ADL");
    let order: Vec<(Address, u64)> = adls(&logs).iter().map(|a| (a.1, a.3)).collect();
    assert_eq!(order, vec![(KEEPER, 1), (ALICE, 2), (BOB, 2)]);
    assert!(adls(&logs).iter().all(|a| a.4 > px(66) && a.4 <= px(67)));
}

/// **R5 — the clean tier is decided PER FILL, on the `take`.** CAROL's ISOLATED 3x long of 2 is
/// liquidated at $60 (bankruptcy ≈ $66.67, $6.67 a unit over the mark). ALICE (CROSSED) is short 10
/// with `E = $40` and the higher rank (`+$10 / $40`): closing her WHOLE short at the fill price
/// would take $66.7 off an equity of $40 — the P2 interim's whole-position test put her in the
/// fallback tier — but this fill closes only the 2 CAROL has, $13.3 of it, which she carries. So
/// she is ADL'd first; KEEPER (ISOLATED, short 2 from $62, ROE `+$4 / $128`) is not reached.
#[test]
fn adl_clean_tier_is_judged_on_the_fill_not_the_whole_position() {
    let mut ctx = make_ctx();
    set_leverage(&mut ctx, CAROL, M1, 3);
    set_leverage(&mut ctx, KEEPER, M1, 1);
    trade(&mut ctx, M1, CAROL, MAKER, BUY, px(100), 2);
    trade_at(&mut ctx, M1, MAKER, ALICE, BUY, px(61), 10);
    trade_at(&mut ctx, M1, MAKER, KEEPER, BUY, px(62), 2);
    assert_eq!(pos(&mut ctx, MAKER, M1).amount, 10);
    set_wallet(&mut ctx, ALICE, usd(30));
    set_mark(&mut ctx, M1, px(60));
    let ledger = Ledger::open(&mut ctx);
    assert_eq!(fold(&mut ctx, ALICE).e, usd(40) as i128);

    let logs = ok(&mut ctx, ADMIN, &liquidate(CAROL, M1));
    ledger.check(&mut ctx, "after the liquidation + ADL");
    let order: Vec<(Address, u64)> = adls(&logs).iter().map(|a| (a.1, a.3)).collect();
    assert_eq!(order, vec![(ALICE, 2)]);
    assert_eq!(pos(&mut ctx, KEEPER, M1).amount, -2, "not reached");
    assert!(wallet(&mut ctx, ALICE) > 0);
}

/// **R5 — a fill the account cannot carry goes behind the fallback tier.** The same scene with
/// CAROL long 8 (bankruptcy ≈ $66.67 again): ALICE's fill would now be 8 — $53.3 against `E = $40`
/// — so she is re-queued behind the fallback tier and KEEPER's clean 2 go first; the rest (6) can
/// only come from ALICE, who takes it as fallback tier.
#[test]
fn adl_a_cross_fill_the_account_cannot_carry_goes_behind_the_clean_tier() {
    let mut ctx = make_ctx();
    set_leverage(&mut ctx, CAROL, M1, 3);
    set_leverage(&mut ctx, KEEPER, M1, 1);
    trade(&mut ctx, M1, CAROL, MAKER, BUY, px(100), 8);
    trade_at(&mut ctx, M1, MAKER, ALICE, BUY, px(61), 10);
    trade_at(&mut ctx, M1, MAKER, KEEPER, BUY, px(62), 2);
    assert_eq!(pos(&mut ctx, MAKER, M1).amount, 4);
    set_wallet(&mut ctx, ALICE, usd(30));
    set_mark(&mut ctx, M1, px(60));
    let ledger = Ledger::open(&mut ctx);

    let logs = ok(&mut ctx, ADMIN, &liquidate(CAROL, M1));
    ledger.check(&mut ctx, "after the liquidation + ADL");
    let order: Vec<(Address, u64)> = adls(&logs).iter().map(|a| (a.1, a.3)).collect();
    assert_eq!(order, vec![(KEEPER, 2), (ALICE, 6)]);
}

// ── Negative-balance clearance (Q8, R4) ─────────────────────────────────────────

/// `AccountBalanceChanged` reason 7, Binance's `INSURANCE_CLEAR`.
const INSURANCE_CLEAR: u8 = 7;

/// `user`'s `AccountBalanceChanged` rows in `logs`, in order.
fn rows(logs: &[Log], user: Address) -> Vec<AccountBalanceChanged> {
    evs::<AccountBalanceChanged>(logs).into_iter().filter(|r| r.user == user).collect()
}

/// **Q8 — the clearance, and its shape.** `user` closed its LAST position in the call that logged
/// `logs`, leaving the wallet at `-deficit`: the insurance fund paid it (`InsuranceFundChanged`,
/// `−deficit`), the wallet is exactly 0, and the account's LAST row of the call is its own
/// `INSURANCE_CLEAR` at 0 — preceded by a row that published the deficit itself, unclamped.
fn assert_cleared(ctx: &mut TestCtx, logs: &[Log], user: Address, deficit: i64, if_before: u64) {
    assert!(deficit > 0);
    assert_eq!(storage::load_account(ctx, user).unwrap().open_position_count, 0, "flat");
    assert_eq!(wallet(ctx, user), 0, "the negative wallet was cleared to exactly 0");
    assert_eq!(if_before - insurance_fund(ctx), deficit as u64, "the fund paid the deficit");
    let changed = evs::<InsuranceFundChanged>(logs);
    assert_eq!(
        changed.last().map(|c| (c.delta, c.newBalance)),
        Some((-deficit, insurance_fund(ctx)))
    );
    assert!(evs::<InsuranceFundDepleted>(logs).is_empty(), "the fund covered it all");
    let r = rows(logs, user);
    let clears: Vec<&AccountBalanceChanged> =
        r.iter().filter(|r| r.reason == INSURANCE_CLEAR).collect();
    assert_eq!(clears.len(), 1, "one INSURANCE_CLEAR row");
    let last = r.last().unwrap();
    assert_eq!(
        (last.reason, last.totalCrossWalletBalance, last.totalWalletBalance),
        (INSURANCE_CLEAR, 0, 0),
        "the clearance is the account's last row of the call"
    );
    let published: Vec<i64> = r.iter().map(|r| r.totalCrossWalletBalance).collect();
    assert!(
        published.contains(&-deficit),
        "the deficit itself was published before it was cleared: {published:?}"
    );
}

/// ALICE: a CROSSED 3x long of 10 on M1 at $100 with $200 left in the wallet — her only position.
/// At a $70 mark she is $100 under water.
fn underwater_long(ctx: &mut TestCtx) {
    long_10_on_m1(ctx, usd(200));
    set_mark(ctx, M1, px(70));
}

/// **Route: the user's own close.** ALICE sells her last position herself (reduce-only, a pure
/// reduce on a cross wallet is never margin-gated) at $70 — through her bankruptcy price: the $300
/// loss settles into the cross wallet (no per-fill bad debt, G2), which goes to −$100, and the end
/// of the call clears it.
#[test]
fn a_user_close_of_the_last_position_clears_the_negative_wallet() {
    let mut ctx = make_ctx();
    underwater_long(&mut ctx);
    rest(&mut ctx, MAKER, M1, BUY, px(70), 10);
    let if0 = insurance_fund_seeded(&mut ctx, usd(1_000));
    let ledger = Ledger::open(&mut ctx);

    let logs = ok(&mut ctx, ALICE, &order_flags(M1, SELL, px(70), 10, IOC, RO));
    ledger.check(&mut ctx, "after the close");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    assert_cleared(&mut ctx, &logs, ALICE, usd(100), if0);
}

/// **Route: a maker fill.** ALICE's last position is closed by her own resting reduce-only order,
/// filled by a taker at $70.
#[test]
fn a_maker_close_of_the_last_position_clears_the_negative_wallet() {
    let mut ctx = make_ctx();
    long_10_on_m1(&mut ctx, usd(200));
    rest_flags(&mut ctx, ALICE, M1, SELL, px(70), 10, RO);
    set_mark(&mut ctx, M1, px(70));
    let if0 = insurance_fund_seeded(&mut ctx, usd(1_000));
    let ledger = Ledger::open(&mut ctx);

    let logs = ok(&mut ctx, BOB, &order(M1, BUY, px(70), 10, IOC));
    ledger.check(&mut ctx, "after the fill");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    assert_cleared(&mut ctx, &logs, ALICE, usd(100), if0);
}

/// **Route: a fired conditional.** ALICE's stop-market (reduce-only) at $75 fires on M1's tick to
/// $70 — BEFORE the liquidation sweep, which then finds no position — and sells into the $70 bid.
#[test]
fn a_fired_stop_closing_the_last_position_clears_the_negative_wallet() {
    let mut ctx = make_ctx();
    long_10_on_m1(&mut ctx, usd(200));
    ok(
        &mut ctx,
        ALICE,
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: M1,
            side: SELL,
            orderType: 1,
            triggerPrice: px(75),
            price: 0,
            quantity: 10,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: RO,
        }
        .abi_encode(),
    );
    rest(&mut ctx, MAKER, M1, BUY, px(70), 10);
    let if0 = insurance_fund_seeded(&mut ctx, usd(1_000));
    let ledger = Ledger::open(&mut ctx);

    let logs = tick(&mut ctx, M1, px(70));
    ledger.check(&mut ctx, "after the tick");
    assert!(evs::<Liquidation>(&logs).is_empty(), "the stop closed it first");
    assert!(evs::<CrossLiquidationStarted>(&logs).is_empty());
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    assert_cleared(&mut ctx, &logs, ALICE, usd(100), if0);
}

/// **Route: a cross liquidation episode (the sweep).** M1's band is 10%: at a $70 mark ALICE's
/// bankruptcy price ($80: `E = −$100`, all of the share on M1) is above the band, so the IOC's
/// limit is clamped DOWN to the band's edge, $77, and fills the $77 bid — below her bankruptcy
/// price. The close realizes −$230, the wallet ends at −$30, the episode ends (no cross position
/// left), and the end of the call clears the rest. (A bankrupt account pays no clearance fee.)
#[test]
fn a_cross_episode_closing_through_the_band_clears_the_negative_wallet() {
    let mut ctx = make_ctx();
    set_fee_rate(&mut ctx, M1, 125);
    let mut mk = storage::load_market(&mut ctx, M1).unwrap().unwrap();
    mk.price_band_bps = 1_000;
    storage::save_market(&mut ctx, &mk).unwrap();
    long_10_on_m1(&mut ctx, usd(200));
    rest(&mut ctx, MAKER, M1, BUY, px(77), 10);
    let if0 = insurance_fund_seeded(&mut ctx, usd(1_000));
    let ledger = Ledger::open(&mut ctx);

    let logs = tick(&mut ctx, M1, px(70));
    ledger.check(&mut ctx, "after the tick");
    let close: Vec<OrderPlaced> =
        evs::<OrderPlaced>(&logs).into_iter().filter(|o| o.user == ALICE).collect();
    assert_eq!(close[0].price, px(77), "clamped from $80 to mark + 10%");
    assert_eq!(evs::<Liquidation>(&logs)[0].reward, 0, "bankrupt: no fee");
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
    assert!(!flagged(&mut ctx, ALICE));
    assert_cleared(&mut ctx, &logs, ALICE, usd(30), if0);
}

/// **Route: the drain.** The same band-clamped episode, but run out of budget in its cancel phase
/// first (ALICE has orders on M2 and M3): the continuation is on M1, and `drainDeferredWork(M1)`
/// resumes it — cancels the rest, closes M1 through the band — and its call clears the wallet.
#[test]
fn a_drained_episode_closing_the_last_position_clears_the_negative_wallet() {
    let mut ctx = make_ctx();
    let mut mk = storage::load_market(&mut ctx, M1).unwrap().unwrap();
    mk.price_band_bps = 1_000;
    storage::save_market(&mut ctx, &mk).unwrap();
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    rest(&mut ctx, ALICE, M2, BUY, px(50), 1);
    rest(&mut ctx, ALICE, M3, BUY, px(50), 1);
    set_wallet(&mut ctx, ALICE, usd(200));
    set_mark(&mut ctx, M1, px(70));
    rest(&mut ctx, MAKER, M1, BUY, px(77), 10);
    let if0 = insurance_fund_seeded(&mut ctx, usd(1_000));
    let ledger = Ledger::open(&mut ctx);

    let caller = EpisodeCaller { sweep_market: None, fresh_market: None, liquidator: KEEPER };
    let (mut budget, mut adl_budget, mut deferred) = (1, 128, false);
    liquidate_cross_account(
        &mut ctx,
        ALICE,
        M1,
        caller,
        &mut budget,
        &mut adl_budget,
        &mut deferred,
    )
    .unwrap();
    storage::flush_account_snapshots(&mut ctx).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());
    assert!(flagged(&mut ctx, ALICE));
    assert_eq!(storage::load_market(&mut ctx, M1).unwrap().unwrap().deferred_work, 1);

    let logs = ok(&mut ctx, KEEPER, &drainDeferredWorkCall { marketId: M1 }.abi_encode());
    ledger.check(&mut ctx, "after the drain");
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, M3).unwrap(), 0);
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
    assert!(!flagged(&mut ctx, ALICE));
    assert_cleared(&mut ctx, &logs, ALICE, usd(30), if0);
}

/// **Route: ADL, the account on the WINNING side.** ALICE (CROSSED, short 4 at $100, wallet −$140)
/// is the only counterparty of CAROL's ISOLATED 3x long, liquidated (by `liquidate()`, so the
/// tick's sweep does not run ALICE's own episode first) at a $60 mark with an empty book. CAROL is
/// insolvent, so ADL fills at her bankruptcy price ≈ $66.67 — $6.67 above the mark against ALICE,
/// more than her `E = $20` can carry (`20 − 4 · 6.67 < 0`, R5), so she is fallback tier, and the
/// only one: she is closed out of her last position with the wallet still below zero, and the end
/// of the call clears it.
#[test]
fn an_adl_fill_closing_the_winners_last_position_clears_its_negative_wallet() {
    let mut ctx = make_ctx();
    set_leverage(&mut ctx, CAROL, M1, 3);
    trade(&mut ctx, M1, CAROL, ALICE, BUY, px(100), 4);
    set_wallet(&mut ctx, ALICE, usd(-140));
    set_mark(&mut ctx, M1, px(60));
    let if0 = insurance_fund_seeded(&mut ctx, usd(1_000));
    let ledger = Ledger::open(&mut ctx);
    assert_eq!(fold(&mut ctx, ALICE).e, usd(20) as i128);

    let logs = ok(&mut ctx, KEEPER, &liquidate(CAROL, M1));
    ledger.check(&mut ctx, "after the liquidation + ADL");
    let a = adls(&logs);
    assert_eq!(a.len(), 1);
    assert_eq!((a[0].0, a[0].1, a[0].3), (CAROL, ALICE, 4));
    assert!(a[0].4 > px(66) && a[0].4 <= px(67), "at CAROL's bankruptcy price: {}", a[0].4);
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
    let paid = if0 - insurance_fund(&mut ctx);
    assert!(paid > 0 && paid < usd(7) as u64, "≈ $6.67: {paid}");
    assert_cleared(&mut ctx, &logs, ALICE, paid as i64, if0);
}

/// **Not before the LAST position (Q8, J3).** ALICE closes her cross long at a loss while she still
/// holds an ISOLATED long on M2: the wallet stays negative — a receivable — and keeps every open
/// and every withdrawal shut (`A < 0`, `X = 0`), across calls. Closing the isolated position hands
/// its silo back to that wallet first; only what is left negative after the LAST close is cleared.
/// Run twice: a silo that covers the deficit (nothing to clear, no row) and one that does not.
#[test]
fn a_negative_wallet_waits_for_the_last_position_and_nets_the_silo_first() {
    for (close_at, residual) in [(px(100), 0), (px(90), usd(10))] {
        let mut ctx = make_ctx();
        set_type(&mut ctx, ALICE, M2, ISOLATED);
        set_leverage(&mut ctx, ALICE, M2, 1);
        trade(&mut ctx, M2, ALICE, CAROL, BUY, px(100), 1);
        underwater_long(&mut ctx);
        rest(&mut ctx, MAKER, M1, BUY, px(70), 10);
        let if0 = insurance_fund_seeded(&mut ctx, usd(1_000));
        let ledger = Ledger::open(&mut ctx);

        let logs = ok(&mut ctx, ALICE, &order_flags(M1, SELL, px(70), 10, IOC, RO));
        ledger.check(&mut ctx, "after the cross close");
        assert_eq!(wallet(&mut ctx, ALICE), usd(-100), "still owed: an isolated position is open");
        assert!(rows(&logs, ALICE).iter().all(|r| r.reason != INSURANCE_CLEAR));
        assert_eq!(insurance_fund(&mut ctx), if0);
        rejects(
            &mut ctx,
            ALICE,
            &transferFromPerpCall { amount: 1 }.abi_encode(),
            "transferFromPerp: insufficient perp wallet balance",
        );
        let out = call(&mut ctx, ALICE, &order(M3, BUY, px(100), 1, GTC));
        assert!(out.reverted, "an opening order on A < 0");
        let _ = JournalTr::take_logs(ctx.journal_mut());
        assert_eq!(wallet(&mut ctx, ALICE), usd(-100), "and it stands across calls");

        // The isolated close: its $100 silo, plus or minus its PnL, nets against the deficit.
        rest(&mut ctx, MAKER, M2, BUY, close_at, 1);
        let logs = ok(&mut ctx, ALICE, &order_flags(M2, SELL, close_at, 1, IOC, RO));
        ledger.check(&mut ctx, "after the isolated close");
        assert_eq!(storage::load_account(&mut ctx, ALICE).unwrap().open_position_count, 0);
        if residual == 0 {
            assert_eq!(wallet(&mut ctx, ALICE), 0, "the silo covered it");
            assert!(rows(&logs, ALICE).iter().all(|r| r.reason != INSURANCE_CLEAR));
            assert!(evs::<InsuranceFundChanged>(&logs).is_empty(), "nothing to clear");
            assert_eq!(insurance_fund(&mut ctx), if0);
        } else {
            assert_cleared(&mut ctx, &logs, ALICE, residual, if0);
        }
    }
}

/// **R4 — the fund is SHORT.** The user-close route with only $40 in the fund: it pays what it has,
/// `InsuranceFundDepleted(M1, $60)` records the rest as socialised (the convention of every deficit
/// the fund cannot absorb), and the wallet still ends at exactly 0 — the identity opens by exactly
/// that $60. With the fund EMPTY the same close writes the fund nothing, logs no
/// `InsuranceFundChanged`, and socialises all $100.
#[test]
fn a_short_insurance_fund_pays_what_it_has_and_logs_the_rest_as_depleted() {
    for fund in [usd(40), 0] {
        let mut ctx = make_ctx();
        underwater_long(&mut ctx);
        rest(&mut ctx, MAKER, M1, BUY, px(70), 10);
        storage::save_insurance_fund(&mut ctx, fund as u64).unwrap();
        let mut ledger = Ledger::open(&mut ctx);

        let logs = ok(&mut ctx, ALICE, &order_flags(M1, SELL, px(70), 10, IOC, RO));
        let uncovered = usd(100) - fund;
        ledger.total += uncovered as i128;
        ledger.check(&mut ctx, "after the close (the uncovered part socialised)");
        assert_eq!(wallet(&mut ctx, ALICE), 0);
        assert_eq!(insurance_fund(&mut ctx), 0);
        let changed = evs::<InsuranceFundChanged>(&logs);
        if fund > 0 {
            assert_eq!(changed.len(), 1);
            assert_eq!((changed[0].delta, changed[0].newBalance), (-fund, 0));
        } else {
            assert!(changed.is_empty(), "nothing paid, nothing logged");
        }
        let depleted = evs::<InsuranceFundDepleted>(&logs);
        assert_eq!(depleted.len(), 1);
        assert_eq!((depleted[0].marketId, depleted[0].badDebt), (M1, uncovered as u64));
        let last = rows(&logs, ALICE).pop().unwrap();
        assert_eq!((last.reason, last.totalCrossWalletBalance), (INSURANCE_CLEAR, 0));
    }
}

/// A last close that leaves the wallet at or above zero clears nothing: no fund write, no event,
/// no extra row.
#[test]
fn a_last_close_with_a_non_negative_wallet_clears_nothing() {
    let mut ctx = make_ctx();
    long_10_on_m1(&mut ctx, usd(300));
    set_mark(&mut ctx, M1, px(70));
    rest(&mut ctx, MAKER, M1, BUY, px(70), 10);
    let if0 = insurance_fund_seeded(&mut ctx, usd(1_000));

    let logs = ok(&mut ctx, ALICE, &order_flags(M1, SELL, px(70), 10, IOC, RO));
    assert_eq!(storage::load_account(&mut ctx, ALICE).unwrap().open_position_count, 0);
    assert_eq!(wallet(&mut ctx, ALICE), 0, "exactly zero is not negative");
    assert_eq!(insurance_fund(&mut ctx), if0);
    assert!(evs::<InsuranceFundChanged>(&logs).is_empty());
    assert!(rows(&logs, ALICE).iter().all(|r| r.reason != INSURANCE_CLEAR));
}

// ── J4: total ───────────────────────────────────────────────────────────────────

/// **J4 — the episode is TOTAL.** ALICE holds cross longs on all three markets; M3's mark is 0 (its
/// leg counts `uPnL = MM = 0`, nothing to close at) and M2's pending funding is past `i64` (the fold
/// leaves that leg out). M1's tick takes the account under maintenance: the sweep closes M1 and
/// skips both — no `Err`, no panic — and the skipped legs are exactly as they were.
#[test]
fn the_episode_skips_a_zero_mark_and_an_unpriceable_leg_without_failing() {
    let mut ctx = make_ctx();
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
/// unless it is still flagged (the episode could not finish) or another account's liquidation
/// reached it after its own turn (an ADL fill, a maker fill of a liquidation close — the next tick
/// catches it), and after EVERY step no account is flagged without a cross position. Coverage
/// floors: cross episodes, bankrupt ones, and multi-market closes are reached.
#[test]
fn a_randomised_multi_market_run_never_panics_and_detection_is_complete() {
    let mut ctx = make_ctx();
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
                    // Reached by another account's liquidation after its own turn in the sweep —
                    // an ADL fill, or a maker fill of a liquidation close (D13: a cross maker fill
                    // is not account-checked) — it is caught by the next tick of its markets.
                    let reached = adls(&logs).iter().any(|a| a.1 == u)
                        || evs::<Trade>(&logs).iter().any(|t| t.maker == u);
                    let f = fold(&mut ctx, u);
                    assert!(
                        !f.below_cross_maintenance() || flagged(&mut ctx, u) || reached,
                        "step {step}: {u} holds a cross position in market {m} and was left \
                         below maintenance, unflagged, by its tick: {f:?}"
                    );
                }
            }
        }
        ledger.check(&mut ctx, &format!("step {step}"));
        // The flag never outlives the account's last cross position, whatever closed it.
        for &u in &traders {
            if flagged(&mut ctx, u) {
                assert!(
                    MARKETS.iter().any(|&m| {
                        let p = pos(&mut ctx, u, m);
                        p.amount != 0
                            && crate::margin_type::effective_margin_type(&p) == MarginType::Crossed
                    }),
                    "step {step}: {u} is flagged with no cross position"
                );
            }
        }
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

/// **TOTAL under stress: deep negative wallets and an EMPTY insurance fund.** A randomised run on
/// three markets with a moving block clock (so marks go stale and episodes skip them), a live 15%
/// band, the clearance fee on M2 only, and the insurance fund held at 0 (re-emptied every 100
/// steps): ALICE and BOB CROSSED everywhere, CAROL CROSSED on M1/M2 and ISOLATED on M3, KEEPER
/// ISOLATED. Each step is a trade, a reduce-only close of a whole position, a resting order
/// (reduce-only or not), a fixture that drives a position holder's wallet deep below zero (to
/// −$5,000), a keeper's `liquidate()` of a random (account, market), a `drainDeferredWork`, or an
/// oracle tick moving a market up to ±40%.
///
/// After EVERY step:
/// * no panic — every sweep, episode, ADL and end-of-call settlement is total;
/// * a refused call wrote nothing (commit-only);
/// * the custody identity holds, opened by exactly what `InsuranceFundDepleted` says the empty fund
///   could not pay (isolated bad debt and cleared negative wallets) — nothing else creates value;
/// * `Σ amount == 0` per market, and no CROSSED position holds a silo;
/// * a FLAT account never has a negative wallet (Q8: cleared at the end of the call that made it
///   flat), and a flagged account always holds a cross position.
///
/// After every tick of `m`: no account holding a cross position in `m` is below maintenance and
/// unflagged, unless an ADL fill of that very tick reached it after its own turn in the sweep.
/// Coverage floors (about half of what this seed reaches): episodes, bankrupt episodes, wallets
/// cleared, socialised deficits, stale markets skipped.
#[test]
fn a_randomised_run_with_deep_negative_wallets_and_an_empty_fund_never_panics() {
    let mut ctx = make_ctx();
    set_fee_rate(&mut ctx, M2, 125);
    for m in MARKETS {
        let mut mk = storage::load_market(&mut ctx, m).unwrap().unwrap();
        mk.price_band_bps = 1_500;
        storage::save_market(&mut ctx, &mk).unwrap();
    }
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, CROSSED);
    }
    set_type(&mut ctx, CAROL, M1, CROSSED);
    set_type(&mut ctx, CAROL, M2, CROSSED);
    let traders = [ALICE, BOB, CAROL, KEEPER];
    let mut rng = Rng(0x5eed_0b3b_c1ea_7001);
    for &u in &traders {
        set_wallet(&mut ctx, u, usd(2_000));
        for m in MARKETS {
            set_leverage(&mut ctx, u, m, 1 + rng.below(3));
        }
    }
    let mut now = 1_000u64;
    ctx.block.timestamp = U256::from(now);
    for m in MARKETS {
        tick_at(&mut ctx, m, px(100), now);
    }
    storage::save_insurance_fund(&mut ctx, 0).unwrap();
    let mut ledger = Ledger::open(&mut ctx);
    let mut index = [px(100); 3];
    let (mut episodes, mut bankrupt, mut cleared, mut socialised, mut stale_skips) =
        (0u32, 0u32, 0u32, 0u32, 0u32);

    // One call: a refusal must not write; an accepted call's logs are collected.
    fn run(ctx: &mut TestCtx, caller: Address, input: &[u8], logs: &mut Vec<Log>) -> bool {
        let writes = ctx.perp_write_count();
        let out = call(ctx, caller, input);
        let l = JournalTr::take_logs(ctx.journal_mut());
        if out.reverted {
            let reason = revert_reason(&out);
            assert_eq!(ctx.perp_write_count(), writes, "a refused call wrote: {reason}");
            assert!(l.is_empty());
            return false;
        }
        logs.extend(l);
        true
    }
    fn withdraw_quotes(ctx: &mut TestCtx, m: u64, logs: &mut Vec<Log>) {
        let ids: Vec<[u8; 32]> = storage::load_buy_orders(ctx, MAKER, m)
            .unwrap()
            .iter()
            .chain(storage::load_sell_orders(ctx, MAKER, m).unwrap().iter())
            .map(|e| e.order_id)
            .collect();
        for id in ids {
            let cancel =
                crate::interface::IPerpDex::cancelOrderCall { orderId: id.into(), marketId: m };
            assert!(run(ctx, MAKER, &cancel.abi_encode(), logs));
        }
    }

    for step in 0..2_000u32 {
        now += 3;
        ctx.block.timestamp = U256::from(now);
        if step % 100 == 0 {
            ledger.total -= insurance_fund(&mut ctx) as i128;
            storage::save_insurance_fund(&mut ctx, 0).unwrap();
        }
        let m_i = rng.below(3) as usize;
        let m = MARKETS[m_i];
        let mark = storage::load_mark_price(&mut ctx, m).unwrap();
        let mut logs = Vec::new();
        let mut ticked = None;
        match rng.below(20) {
            0..=8 => {
                // A trade against a fresh MAKER quote near the mark, sized off available balance.
                let u = traders[rng.below(4) as usize];
                let side = rng.below(2) as u8;
                let lev = pos(&mut ctx, u, m).leverage.max(1) as u128;
                let a = fold(&mut ctx, u).a.max(0) as u128;
                let affordable = (a * lev / (mark as u128 * 10_000)).min(200) as u64;
                let qty = 1 + rng.below(affordable.max(1));
                let p = if side == BUY {
                    mark + rng.below(mark / 20 + 1)
                } else {
                    mark.saturating_sub(rng.below(mark / 20 + 1)).max(1)
                };
                assert!(run(&mut ctx, MAKER, &order(m, 1 - side, p, qty, GTC), &mut logs));
                run(&mut ctx, u, &order(m, side, p, qty, IOC), &mut logs);
                withdraw_quotes(&mut ctx, m, &mut logs);
            }
            9..=10 => {
                // A trader closes a whole position with a reduce-only IOC, at whatever the book
                // gives.
                let u = traders[rng.below(4) as usize];
                let amount = pos(&mut ctx, u, m).amount;
                if amount != 0 {
                    let (side, qty) = (if amount > 0 { SELL } else { BUY }, amount.unsigned_abs());
                    let p = if side == SELL {
                        mark.saturating_sub(rng.below(mark / 10 + 1)).max(1)
                    } else {
                        mark + rng.below(mark / 10 + 1)
                    };
                    assert!(run(&mut ctx, MAKER, &order(m, 1 - side, p, qty, GTC), &mut logs));
                    run(&mut ctx, u, &order_flags(m, side, p, qty, IOC, RO), &mut logs);
                    withdraw_quotes(&mut ctx, m, &mut logs);
                }
            }
            11 => {
                // A resting order of a trader — reduce-only half the time — left for the episode.
                let u = traders[rng.below(4) as usize];
                let side = rng.below(2) as u8;
                let p = if side == BUY { mark * 9 / 10 } else { mark * 11 / 10 };
                let flags = if rng.below(2) == 0 { RO } else { 0 };
                let input = order_flags(m, side, p.max(1), 1 + rng.below(3), GTC, flags);
                run(&mut ctx, u, &input, &mut logs);
            }
            12 => {
                // Fixture: a position holder's wallet driven deep below zero (value leaves the
                // system — the identity's total moves by exactly it).
                let u = traders[rng.below(4) as usize];
                if storage::load_account(&mut ctx, u).unwrap().open_position_count > 0 {
                    let old = wallet(&mut ctx, u);
                    let new = -usd(rng.below(5_000) as i64);
                    set_wallet(&mut ctx, u, new);
                    ledger.total += new as i128 - old as i128;
                }
            }
            13 => {
                let u = traders[rng.below(4) as usize];
                run(&mut ctx, ADMIN, &liquidate(u, m), &mut logs);
            }
            14 => {
                let drain = drainDeferredWorkCall { marketId: m }.abi_encode();
                run(&mut ctx, KEEPER, &drain, &mut logs);
            }
            _ => {
                let d = index[m_i] * rng.below(41) / 100;
                index[m_i] =
                    if rng.below(2) == 0 { index[m_i] + d } else { index[m_i].saturating_sub(d) }
                        .max(px(5));
                let last = storage::load_index_price_state(&mut ctx, m).unwrap().timestamp;
                let ts = now.max(last + 15);
                logs.extend(tick_at(&mut ctx, m, index[m_i], ts));
                ticked = Some(m);
            }
        }

        for d in evs::<InsuranceFundDepleted>(&logs) {
            ledger.total += d.badDebt as i128;
            socialised += 1;
        }
        ledger.check(&mut ctx, &format!("step {step}"));
        let started = evs::<CrossLiquidationStarted>(&logs);
        episodes += started.len() as u32;
        bankrupt += started.iter().filter(|s| s.crossMarginBalance < 0).count() as u32;
        cleared += evs::<AccountBalanceChanged>(&logs)
            .iter()
            .filter(|r| r.reason == INSURANCE_CLEAR && r.user != ADMIN)
            .count() as u32;
        for &u in &traders {
            let acct = storage::load_account(&mut ctx, u).unwrap();
            if acct.open_position_count == 0 {
                assert!(
                    acct.perp_wallet_balance >= 0,
                    "step {step}: {u} is flat at a negative wallet"
                );
            }
            let cross_positions: Vec<u64> = MARKETS
                .iter()
                .copied()
                .filter(|&k| {
                    let p = pos(&mut ctx, u, k);
                    p.amount != 0
                        && crate::margin_type::effective_margin_type(&p) == MarginType::Crossed
                })
                .collect();
            if acct.in_liquidation() {
                assert!(
                    !cross_positions.is_empty(),
                    "step {step}: {u} flagged with no cross position"
                );
                // A flagged account below maintenance holding a STALE market the episode skipped.
                stale_skips += cross_positions
                    .iter()
                    .filter(|&&k| {
                        let ts = storage::load_index_price_state(&mut ctx, k).unwrap().timestamp;
                        now.saturating_sub(ts) > crate::risk::cross_liquidation::STALE_MULT * 15
                    })
                    .count() as u32;
            }
            if let Some(t) = ticked {
                // Reached by another account's liquidation after its own turn in the sweep: an
                // ADL fill, or a maker fill of a liquidation close (D13 — a cross maker fill is not
                // account-checked; the next tick of any of its markets catches it).
                let reached = adls(&logs).iter().any(|a| a.1 == u)
                    || evs::<Trade>(&logs).iter().any(|t| t.maker == u);
                if cross_positions.contains(&t) && !reached {
                    let f = fold(&mut ctx, u);
                    assert!(
                        !f.below_cross_maintenance() || acct.in_liquidation(),
                        "step {step}: {u} holds a cross position in market {t} and was left below \
                         maintenance, unflagged, by its tick: {f:?}"
                    );
                }
            }
        }
        // Top up a FLAT, drained account so it keeps trading.
        for &u in &traders {
            let flat = storage::load_account(&mut ctx, u).unwrap().open_position_count == 0;
            if flat && wallet(&mut ctx, u) < usd(200) {
                deposit(&mut ctx, &mut ledger, u, usd(2_000));
            }
        }
    }
    eprintln!(
        "coverage: episodes {episodes}, bankrupt {bankrupt}, wallets cleared {cleared}, socialised \
         {socialised}, flagged-on-stale step-accounts {stale_skips}"
    );
    // This seed reaches 73 / 53 / 54 / 47 / 142. Across 120 seeds the minima were 42 / 34 / 36 /
    // 32 / 36, and no seed failed any per-step property.
    assert!(episodes >= 30, "episodes {episodes}");
    assert!(bankrupt >= 20, "bankrupt episodes {bankrupt}");
    assert!(cleared >= 20, "wallets cleared {cleared}");
    assert!(socialised >= 20, "socialised deficits {socialised}");
    assert!(stale_skips >= 20, "flagged on a stale market {stale_skips}");
}
