//! Cross margin P2b — money movement on a position CROSSED in force.
//!
//! * fills: a cross open moves NO cash (`pos.margin` stays 0), a cross close credits its realized
//!   PnL to the cross wallet `W` EITHER SIGN with no per-fill bad debt, every fee comes out of `W`,
//!   and a flip does both in one fill — taker and maker alike;
//! * funding: a cross position's funding settles into `W`, published as `FundingSettled` plus a
//!   B-only `AccountBalanceChanged(FUNDING_FEE)` group — no `PositionChanged`;
//!
//! Every scenario re-checks the custody identity after every step ([`Ledger::check`]): the
//! price-independent sum `Σ spot + Σ W + Σ margin + Σ vQuote + Σ pending funding + IF` over every
//! account (the fee recipient included) never moves, and `Σ amount == 0` in every market. With
//! `Σ amount == 0` the mark-to-market term `Σ trunc(amount · mark)` is exactly 0 on this grid, so
//! this IS the identity `erc20(USDC, PERP_DEX) == Σ claims` with the deposits held fixed. A negative
//! `W` enters it as a negative claim, which is what keeps it closing.
//!
//! Cross margin is gated off in production; every test here takes the `#[cfg(test)]` override
//! (`margin_type::test_gate`). Under it a position nobody switched is CROSSED (the stored default),
//! so each counterparty that is meant to be ISOLATED is switched explicitly, through
//! `setMarginType`.
use alloy_sol_types::{SolCall, SolError, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, Log, U256};

use crate::{
    interface::IPerpDex::{
        placeOrderCall, setLeverageCall, setMarginTypeCall, AccountBalanceChanged,
        FundingSettled, InsuranceFundChanged, InsuranceFundDepleted, PositionChanged,
    },
    run_perp_dex_call, storage,
    types::{AccountUpdateReason, FundingState, MarginTiers, MarginType, Market, UserAccount},
    PerpOutput, PERP_DEX_ADDRESS, USDC_ADDRESS,
};

// ── Fixture ───────────────────────────────────────────────────────────────────

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
/// Every account that can hold value here — the fee recipient included, it is part of the system.
const USERS: [Address; 4] = [ALICE, BOB, CAROL, ADMIN];

/// Two markets on one grid: base 0 decimals, price 2 decimals, so one contract at a price of `P`
/// cents is worth `P · 10^4` quote units and every number below is exact.
const M1: u64 = 1;
const M2: u64 = 2;
const MARKETS: [u64; 2] = [M1, M2];
/// One dollar in quote units.
const USD: i64 = 1_000_000;
/// The default perp wallet: deep enough that margin never interferes unless a test shrinks it.
const WALLET: i64 = 1_000_000 * USD;
/// Fee rates, ppm of notional: 0.05% taker, 0.02% maker.
const TAKER_PPM: u64 = 500;
const MAKER_PPM: i64 = 200;

const BUY: u8 = 0;
const SELL: u8 = 1;
const GTC: u8 = 0;
const IOC: u8 = 1;
const CROSSED: u8 = 0;
const ISOLATED: u8 = 1;

/// `$d` as a price (cents).
const fn px(dollars: u64) -> u64 {
    dollars * 100
}

/// The value of `qty` contracts at `price` cents, in quote units.
fn value(price: u64, qty: u64) -> i64 {
    (price as i128 * qty as i128 * 10_000) as i64
}

/// `ppm` of `notional`, floored — the engine's `calc_trading_fee` on this grid.
fn fee(notional: i64, ppm: u64) -> i64 {
    (notional as i128 * ppm as i128 / 1_000_000) as i64
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
        // ≥ 100%: the fill-time band is off, so a test moves the mark without dragging the book.
        price_band_bps: 10_000,
        mark_price: px(100),
        tiers: MarginTiers::default(),
        basis_mode: 0,
        deferred_work: 0,
        cumulative_funding_index: 0,
    }
}

fn make_ctx() -> TestCtx {
    let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
    for a in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, BOB, CAROL, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), a).unwrap();
    }
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    for id in MARKETS {
        storage::save_market(&mut ctx, &market(id)).unwrap();
    }
    for u in [ALICE, BOB, CAROL] {
        let a = UserAccount {
            perp_wallet_balance: WALLET,
            maker_fee_ppm: MAKER_PPM,
            taker_fee_ppm: TAKER_PPM,
            ..UserAccount::default()
        };
        storage::save_account(&mut ctx, u, a, AccountUpdateReason::Adjustment).unwrap();
    }
    storage::flush_account_snapshots(&mut ctx).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());
    ctx
}

fn call(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, 30_000_000, caller, U256::ZERO, false, ctx)
        .expect("a perp call must not hard-fail")
}

fn revert_reason(out: &PerpOutput) -> String {
    alloy_sol_types::Revert::abi_decode(&out.bytes).map(|r| r.reason).unwrap_or_default()
}

/// Runs `input` as `caller`, asserts it succeeded, and returns the logs it emitted.
fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> Vec<Log> {
    let out = call(ctx, caller, input);
    assert!(!out.reverted, "call reverted: {}", revert_reason(&out));
    JournalTr::take_logs(ctx.journal_mut())
}

fn order(market: u64, side: u8, price: u64, qty: u64, tif: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: market,
        side,
        price,
        quantity: qty,
        orderType: 0,
        tif,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode()
}

/// `maker` rests a GTC at `price`, `taker` crosses it with an IOC of the same size; `side` is the
/// TAKER's. Returns the taker call's logs.
fn trade(
    ctx: &mut TestCtx,
    market: u64,
    taker: Address,
    maker: Address,
    side: u8,
    price: u64,
    qty: u64,
) -> Vec<Log> {
    ok(ctx, maker, &order(market, 1 - side, price, qty, GTC));
    ok(ctx, taker, &order(market, side, price, qty, IOC))
}

fn set_type(ctx: &mut TestCtx, user: Address, market: u64, margin_type: u8) {
    ok(ctx, user, &setMarginTypeCall { marketId: market, marginType: margin_type }.abi_encode());
}

fn set_leverage(ctx: &mut TestCtx, user: Address, market: u64, leverage: u64) {
    ok(ctx, user, &setLeverageCall { marketId: market, leverage }.abi_encode());
}

fn set_mark(ctx: &mut TestCtx, market: u64, mark: u64) {
    storage::save_mark_price(ctx, market, mark).unwrap();
}

fn set_wallet(ctx: &mut TestCtx, user: Address, w: i64) {
    storage::mutate_account(ctx, user, |a| a.perp_wallet_balance = w).unwrap();
}

fn set_taker_fee(ctx: &mut TestCtx, user: Address, ppm: u64) {
    storage::mutate_account(ctx, user, |a| a.taker_fee_ppm = ppm).unwrap();
}

fn wallet(ctx: &mut TestCtx, user: Address) -> i64 {
    storage::load_account(ctx, user).unwrap().perp_wallet_balance
}

fn pos(ctx: &mut TestCtx, user: Address, market: u64) -> crate::types::PerpPosition {
    storage::load_position(ctx, user, market).unwrap()
}

fn insurance_fund(ctx: &mut TestCtx) -> u64 {
    storage::load_insurance_fund(ctx).unwrap()
}

fn position_changes(logs: &[Log], user: Address) -> Vec<PositionChanged> {
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&PositionChanged::SIGNATURE_HASH))
        .map(|l| PositionChanged::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .filter(|e| e.user == user)
        .collect()
}

fn count_events<E: SolEvent>(logs: &[Log]) -> usize {
    logs.iter().filter(|l| l.data.topics().first() == Some(&E::SIGNATURE_HASH)).count()
}

// ── The custody identity ────────────────────────────────────────────────────────

/// The value the system holds, fixed at construction and re-checked after every step.
///
/// `Σ_users (spot + W + Σ_markets (margin + vQuote + PF)) + IF`, where `PF` is the funding each
/// position has accrued but not settled (priced exactly as settlement will apply it). With
/// `Σ amount == 0` per market the mark-to-market half of the identity vanishes, so this sum is the
/// whole of what the precompile custodies and must never move: a fill, a fee, a funding settle,
/// a liquidation leg or an ADL fill only moves value BETWEEN its terms.
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

    /// The identity, plus `Σ amount == 0` per market and `CROSSED ⇒ margin == 0` on every row.
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
        assert_eq!(
            Self::system_value(ctx),
            self.total,
            "{at}: CUSTODY BROKEN — value was created or destroyed"
        );
    }
}

// ── Fills ─────────────────────────────────────────────────────────────────────

/// A cross TAKER open moves no cash: `pos.margin` stays 0, the wallet loses exactly the taker fee,
/// the fee recipient gains both fees, and the ISOLATED maker on the other side funds its silo as it
/// always did. The per-position K9 does not fire on the margin-free position (at mark == entry its
/// equity is 0, under any maintenance requirement), and the row says CROSSED with no silo.
#[test]
fn a_cross_taker_open_moves_no_cash_and_pays_its_fee_from_the_wallet() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    let ledger = Ledger::open(&mut ctx);

    let logs = trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    ledger.check(&mut ctx, "after the open");

    let notional = value(px(100), 10);
    let a = pos(&mut ctx, ALICE, M1);
    assert_eq!((a.amount, a.v_quote_balance, a.margin), (10, -notional, 0));
    assert_eq!(wallet(&mut ctx, ALICE), WALLET - fee(notional, TAKER_PPM));
    // The isolated maker: silo = notional / 1x, its fee out of the silo, the wallet debited the silo.
    let b = pos(&mut ctx, BOB, M1);
    assert_eq!(b.margin, notional - fee(notional, MAKER_PPM as u64));
    assert_eq!(wallet(&mut ctx, BOB), WALLET - notional);
    assert_eq!(
        wallet(&mut ctx, ADMIN),
        fee(notional, TAKER_PPM) + fee(notional, MAKER_PPM as u64)
    );
    assert_eq!(storage::load_account(&mut ctx, ALICE).unwrap().total_position_margin, 0);

    let row = position_changes(&logs, ALICE);
    assert_eq!(row.len(), 1);
    assert_eq!((row[0].margin, row[0].marginType), (0, CROSSED));
    assert_eq!(count_events::<InsuranceFundChanged>(&logs), 0);
}

/// A cross MAKER open moves no cash either, pays its maker fee from the wallet, and is NOT refused
/// on per-position solvency even when the mark has moved far against it before the fill: its
/// equity read the isolated way would be `uPnL = −$500` against a $83 maintenance requirement, but a
/// cross position's solvency is account-level (owner decision Q9 — no account check on a maker
/// fill either).
#[test]
fn a_cross_maker_open_moves_no_cash_and_is_not_refused_on_per_position_solvency() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    let ledger = Ledger::open(&mut ctx);

    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 10, GTC));
    set_mark(&mut ctx, M1, px(50));
    let logs = ok(&mut ctx, BOB, &order(M1, SELL, px(100), 10, IOC));
    ledger.check(&mut ctx, "after the maker fill");

    let notional = value(px(100), 10);
    let a = pos(&mut ctx, ALICE, M1);
    assert_eq!((a.amount, a.v_quote_balance, a.margin), (10, -notional, 0), "filled, not cancelled");
    assert_eq!(wallet(&mut ctx, ALICE), WALLET - fee(notional, MAKER_PPM as u64));
    let row = position_changes(&logs, ALICE);
    assert_eq!((row.len(), row[0].margin, row[0].marginType), (1, 0, CROSSED));
}

/// A partial close at a profit credits exactly the closed slice's realized PnL to the wallet, the
/// remaining entry basis is prorated, and nothing is ever parked in a silo.
#[test]
fn a_cross_partial_close_credits_the_realized_profit_to_the_wallet() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    set_type(&mut ctx, CAROL, M1, ISOLATED);
    let ledger = Ledger::open(&mut ctx);

    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    let w0 = wallet(&mut ctx, ALICE);
    set_mark(&mut ctx, M1, px(110));
    let logs = trade(&mut ctx, M1, ALICE, CAROL, SELL, px(110), 4);
    ledger.check(&mut ctx, "after the partial close");

    let realized = value(px(110), 4) - value(px(100), 4);
    assert_eq!(realized, 40 * USD);
    assert_eq!(wallet(&mut ctx, ALICE), w0 + realized - fee(value(px(110), 4), TAKER_PPM));
    let a = pos(&mut ctx, ALICE, M1);
    assert_eq!((a.amount, a.v_quote_balance, a.margin), (6, -value(px(100), 6), 0));
    assert_eq!(a.cumulative_realized_pnl, realized);
    let row = position_changes(&logs, ALICE);
    assert_eq!((row[0].realizedPnl, row[0].closedQuantity, row[0].margin), (realized, 4, 0));
}

/// G2: a cross close whose loss exceeds the wallet takes the wallet NEGATIVE. There is no per-fill
/// bad debt — the insurance fund is not touched and no `InsuranceFundDepleted` is emitted — and the
/// counterparty's gain is whole: the loss is a claim on ALICE's account, not a write-off. The
/// maker's fee is charged on top, from the same (now negative) wallet.
///
/// The realistic shape of it: the negative wallet is BACKED by the account's other cross position
/// (M2, deep in profit), which is also what admits the resting close — its Assuming-Price `ooIM`
/// is paid out of cross unrealized profit (`A`), not out of the wallet.
#[test]
fn a_cross_loss_beyond_the_wallet_drives_it_negative_with_no_bad_debt() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
        set_type(&mut ctx, CAROL, m, ISOLATED);
        set_leverage(&mut ctx, ALICE, m, 3);
    }
    set_wallet(&mut ctx, ALICE, 400 * USD);
    let ledger = Ledger::open(&mut ctx);

    // M2: a cross long that triples. M1: a cross long opened on the strength of it.
    trade(&mut ctx, M2, ALICE, BOB, BUY, px(100), 10);
    set_mark(&mut ctx, M2, px(300));
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    ledger.check(&mut ctx, "after both opens");
    let w0 = wallet(&mut ctx, ALICE);
    assert_eq!(w0, 400 * USD - 2 * fee(value(px(100), 10), TAKER_PPM));
    let if0 = insurance_fund(&mut ctx);

    // M1's mark collapses to $40 and ALICE closes it as a MAKER.
    set_mark(&mut ctx, M1, px(40));
    ok(&mut ctx, ALICE, &order(M1, SELL, px(40), 10, GTC));
    let logs = ok(&mut ctx, CAROL, &order(M1, BUY, px(40), 10, IOC));
    ledger.check(&mut ctx, "after the losing close");

    let realized = value(px(40), 10) - value(px(100), 10);
    assert_eq!(realized, -600 * USD);
    let w1 = wallet(&mut ctx, ALICE);
    assert_eq!(w1, w0 + realized - fee(value(px(40), 10), MAKER_PPM as u64));
    assert!(w1 < 0, "the wallet went negative: {w1}");
    let a = pos(&mut ctx, ALICE, M1);
    assert_eq!((a.amount, a.v_quote_balance, a.margin), (0, 0, 0));
    assert_eq!(insurance_fund(&mut ctx), if0, "no per-fill bad debt reaches the fund");
    assert_eq!(count_events::<InsuranceFundDepleted>(&logs), 0);
    assert_eq!(count_events::<InsuranceFundChanged>(&logs), 0);
    assert_eq!(position_changes(&logs, ALICE)[0].realizedPnl, realized);
    // The account is healthy: E = W + M2's +$2,000 is far above Σ MM.
    let f = crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, None).unwrap();
    assert_eq!(f.e, w1 as i128 + 2_000 * USD as i128);
    assert!(!f.below_cross_maintenance());
}

/// The same loss closed as a TAKER (with a zero taker fee — a cross pure close that leaves the
/// available balance negative is still refused its fee, exactly like an isolated one; that gate is
/// cross P2c's): the wallet goes negative by exactly the realized loss.
#[test]
fn a_cross_taker_close_at_a_loss_settles_into_the_wallet_with_no_bad_debt() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    set_type(&mut ctx, CAROL, M1, ISOLATED);
    set_leverage(&mut ctx, ALICE, M1, 3);
    set_wallet(&mut ctx, ALICE, 400 * USD);
    let ledger = Ledger::open(&mut ctx);

    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    let w0 = wallet(&mut ctx, ALICE);
    set_taker_fee(&mut ctx, ALICE, 0);
    set_mark(&mut ctx, M1, px(40));
    let logs = trade(&mut ctx, M1, ALICE, CAROL, SELL, px(40), 10);
    ledger.check(&mut ctx, "after the taker close");

    assert_eq!(wallet(&mut ctx, ALICE), w0 - 600 * USD);
    assert_eq!(count_events::<InsuranceFundDepleted>(&logs), 0);
    assert_eq!(count_events::<InsuranceFundChanged>(&logs), 0);
}

/// A flip in ONE fill: the closing leg realizes into the wallet, the opening leg opens the other
/// side with no cash moved, the fee is charged on the whole notional from the wallet, and the
/// position never holds margin.
#[test]
fn a_cross_flip_closes_and_reopens_in_one_fill() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    set_type(&mut ctx, CAROL, M1, ISOLATED);
    let ledger = Ledger::open(&mut ctx);

    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    let w0 = wallet(&mut ctx, ALICE);
    set_mark(&mut ctx, M1, px(90));
    let logs = trade(&mut ctx, M1, ALICE, CAROL, SELL, px(90), 15);
    ledger.check(&mut ctx, "after the flip");

    let realized = value(px(90), 10) - value(px(100), 10);
    assert_eq!(
        wallet(&mut ctx, ALICE),
        w0 + realized - fee(value(px(90), 15), TAKER_PPM)
    );
    let a = pos(&mut ctx, ALICE, M1);
    assert_eq!((a.amount, a.v_quote_balance, a.margin), (-5, value(px(90), 5), 0));
    let row = position_changes(&logs, ALICE);
    assert_eq!((row[0].amount, row[0].realizedPnl, row[0].closedQuantity), (-5, realized, 10));
}

/// A MIXED account: CROSSED in M1, ISOLATED in M2. The isolated open funds its silo out of the
/// wallet exactly as before (`Σ pos.margin` is that silo alone), the cross open moves nothing, and
/// closing both returns the silo and settles the cross PnL into the same wallet.
#[test]
fn a_mixed_account_funds_its_isolated_silo_and_nothing_for_its_cross_leg() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
        set_type(&mut ctx, CAROL, m, ISOLATED);
    }
    set_type(&mut ctx, ALICE, M2, ISOLATED);
    set_leverage(&mut ctx, ALICE, M2, 2);
    let ledger = Ledger::open(&mut ctx);

    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    trade(&mut ctx, M2, ALICE, BOB, BUY, px(100), 10);
    ledger.check(&mut ctx, "after both opens");
    let n = value(px(100), 10);
    let silo = n / 2 - fee(n, TAKER_PPM);
    assert_eq!(pos(&mut ctx, ALICE, M1).margin, 0);
    assert_eq!(pos(&mut ctx, ALICE, M2).margin, silo);
    assert_eq!(storage::load_account(&mut ctx, ALICE).unwrap().total_position_margin, silo);
    assert_eq!(wallet(&mut ctx, ALICE), WALLET - fee(n, TAKER_PPM) - n / 2);

    set_mark(&mut ctx, M1, px(120));
    set_mark(&mut ctx, M2, px(120));
    trade(&mut ctx, M1, ALICE, CAROL, SELL, px(120), 10);
    trade(&mut ctx, M2, ALICE, CAROL, SELL, px(120), 10);
    ledger.check(&mut ctx, "after both closes");
    let gain = value(px(120), 10) - n;
    let close_fee = fee(value(px(120), 10), TAKER_PPM);
    assert_eq!(
        wallet(&mut ctx, ALICE),
        WALLET - 2 * fee(n, TAKER_PPM) + 2 * gain - 2 * close_fee,
        "both legs realize the same $200 into the one wallet"
    );
    assert_eq!(storage::load_account(&mut ctx, ALICE).unwrap().total_position_margin, 0);
}

// ── Funding ───────────────────────────────────────────────────────────────────

/// Moves `market`'s cumulative funding index (and its `Market` mirror — `save_funding_state` is
/// the one door for both) without a tick.
fn set_funding_index(ctx: &mut TestCtx, market: u64, index: i128) {
    let fs = FundingState { last_funding_rate: 7_500, next_funding_ts: 0, cumulative_funding_index: index };
    storage::save_funding_state(ctx, market, &fs).unwrap();
}

/// The funding a position of `amount` pays (`< 0`) or receives at `index`, from a fresh anchor:
/// `calc_funding_payment` on this grid is `−amount · Δ / 10^2`.
fn funding_payment(amount: i64, index: i128) -> i64 {
    (-(amount as i128) * index / 100) as i64
}

/// The `FundingSettled` rows of `logs` for `user`, each with the index of the log after it.
fn funding_rows(logs: &[Log], user: Address) -> Vec<(usize, FundingSettled)> {
    logs.iter()
        .enumerate()
        .filter(|(_, l)| l.data.topics().first() == Some(&FundingSettled::SIGNATURE_HASH))
        .map(|(i, l)| (i, FundingSettled::decode_raw_log(l.data.topics(), &l.data.data).unwrap()))
        .filter(|(_, e)| e.user == user)
        .collect()
}

fn header_at(logs: &[Log], i: usize) -> Option<AccountBalanceChanged> {
    let l = logs.get(i)?;
    (l.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
        .then(|| AccountBalanceChanged::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
}

fn is_position_row(logs: &[Log], i: usize) -> bool {
    logs.get(i).is_some_and(|l| l.data.topics().first() == Some(&PositionChanged::SIGNATURE_HASH))
}

/// A cross position's funding — paid here — settles into the CROSS WALLET at its next touch (a
/// partial close as a taker), never into a silo, and is published Binance's B-only way:
/// `FundingSettled`, then the `AccountBalanceChanged(FUNDING_FEE)` header carrying the moved wallet,
/// and NO position row in that group. The isolated counterparty's funding, still pending, is
/// untouched; the identity (which counts pending funding) holds throughout.
#[test]
fn cross_funding_settles_into_the_wallet_as_a_balance_only_push() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    set_type(&mut ctx, CAROL, M1, ISOLATED);
    let ledger = Ledger::open(&mut ctx);

    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    // 0.75% of $100 per contract: the long pays $7.50, the short is owed it.
    let index = 10_000 * 7_500;
    set_funding_index(&mut ctx, M1, index);
    ledger.check(&mut ctx, "with funding pending on both sides");
    let owed = funding_payment(10, index);
    assert_eq!(owed, -7_500_000);
    let w0 = wallet(&mut ctx, ALICE);
    let before = crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, None).unwrap();
    assert_eq!(before.cwb, w0 as i128 + owed as i128, "cWB counts it while pending");

    let logs = trade(&mut ctx, M1, ALICE, CAROL, SELL, px(100), 4);
    ledger.check(&mut ctx, "after the settling close");
    crate::events::stream_test_support::assert_account_update_groups(&logs);

    let close_fee = fee(value(px(100), 4), TAKER_PPM);
    assert_eq!(wallet(&mut ctx, ALICE), w0 + owed - close_fee);
    let a = pos(&mut ctx, ALICE, M1);
    assert_eq!((a.amount, a.margin, a.last_funding_index), (6, 0, index));
    // BOB (isolated, untouched) still has his credit pending, on his position.
    assert_eq!(pos(&mut ctx, BOB, M1).last_funding_index, 0);

    let rows = funding_rows(&logs, ALICE);
    assert_eq!(rows.len(), 1, "one settle");
    let (i, settled) = &rows[0];
    assert_eq!(settled.amount, owed);
    let header = header_at(&logs, i + 1).expect("the funding group's header follows FundingSettled");
    assert_eq!(header.user, ALICE);
    assert_eq!(header.reason, AccountUpdateReason::FundingFee.code());
    assert_eq!(header.totalCrossWalletBalance, w0 + owed, "the header carries the moved wallet");
    assert!(!is_position_row(&logs, i + 2), "a cross funding settle publishes NO position row");
}

/// The receiving side, as a MAKER: a cross short is owed funding, which lands on its wallet when
/// the maker is first touched in a match — before its fill, as its own B-only group — and the fill
/// then settles on top of it.
#[test]
fn a_cross_maker_receives_funding_into_the_wallet_before_its_fill() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    set_type(&mut ctx, CAROL, M1, ISOLATED);
    let ledger = Ledger::open(&mut ctx);

    trade(&mut ctx, M1, ALICE, BOB, SELL, px(100), 10); // ALICE cross short 10
    let index = 10_000 * 7_500;
    set_funding_index(&mut ctx, M1, index);
    let owed = funding_payment(-10, index);
    assert_eq!(owed, 7_500_000, "the short is paid");
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 3, GTC));
    let w0 = wallet(&mut ctx, ALICE);
    let logs = ok(&mut ctx, CAROL, &order(M1, SELL, px(100), 3, IOC));
    ledger.check(&mut ctx, "after the maker fill");
    crate::events::stream_test_support::assert_account_update_groups(&logs);

    assert_eq!(wallet(&mut ctx, ALICE), w0 + owed - fee(value(px(100), 3), MAKER_PPM as u64));
    assert_eq!(pos(&mut ctx, ALICE, M1).margin, 0);
    let rows = funding_rows(&logs, ALICE);
    assert_eq!(rows.len(), 1);
    let (i, settled) = &rows[0];
    assert_eq!(settled.amount, owed);
    let header = header_at(&logs, i + 1).unwrap();
    assert_eq!((header.user, header.totalCrossWalletBalance), (ALICE, w0 + owed));
    assert!(!is_position_row(&logs, i + 2));
}

/// A funding charge larger than the wallet takes a cross wallet negative, exactly like a realized
/// loss — no clamp, no insurance-fund leg — and it stays negative (owner decision Q8: clearance is
/// P3's, and only once the account holds no position).
#[test]
fn a_cross_funding_charge_beyond_the_wallet_drives_it_negative() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    set_type(&mut ctx, CAROL, M1, ISOLATED);
    set_leverage(&mut ctx, ALICE, M1, 3);
    set_wallet(&mut ctx, ALICE, 400 * USD);
    let ledger = Ledger::open(&mut ctx);

    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    let w0 = wallet(&mut ctx, ALICE);
    // A (fixture-sized) $500 charge on the $1,000 long.
    let index = 5_000_000_000;
    set_funding_index(&mut ctx, M1, index);
    let owed = funding_payment(10, index);
    assert_eq!(owed, -500 * USD);
    let if0 = insurance_fund(&mut ctx);
    // Settle it with a zero-fee pure close of 1 (a close with a fee would be refused: the
    // available balance is below the fee once the charge lands).
    set_taker_fee(&mut ctx, ALICE, 0);
    let logs = trade(&mut ctx, M1, ALICE, CAROL, SELL, px(100), 1);
    ledger.check(&mut ctx, "after the charge settled");
    assert_eq!(wallet(&mut ctx, ALICE), w0 + owed);
    assert!(wallet(&mut ctx, ALICE) < 0);
    assert_eq!(pos(&mut ctx, ALICE, M1).margin, 0);
    assert_eq!(insurance_fund(&mut ctx), if0);
    assert_eq!(count_events::<InsuranceFundChanged>(&logs), 0);
}
