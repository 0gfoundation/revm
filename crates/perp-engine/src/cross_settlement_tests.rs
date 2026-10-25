//! Cross margin P2b — money movement on a position CROSSED in force.
//!
//! * fills: a cross open moves NO cash (`pos.margin` stays 0), a cross close credits its realized
//!   PnL to the cross wallet `W` EITHER SIGN with no per-fill bad debt, every fee comes out of `W`,
//!   and a flip does both in one fill — taker and maker alike;
//! * funding: a cross position's funding settles into `W`, published as `FundingSettled` plus a
//!   B-only `AccountBalanceChanged(FUNDING_FEE)` group — no `PositionChanged`;
//! * the P2 liquidation interim: the sweep and `liquidate()` never liquidate a CROSSED position
//!   (no `Err`, no write), and ADL ranks a cross winner on its account `E` and settles it into `W`;
//! * J6: the cash a TAKER moves into an ISOLATED silo must be in the wallet — cross unrealized
//!   profit admits orders, it never becomes isolated collateral; the maker's M1 cap is unchanged;
//! * the negative-wallet audit: every money-out gate reads `X`, so a negative cross wallet (or
//!   cross profit) never leaves; a randomised mixed sequence keeps the identity after every step;
//! * gated off, every one of those paths is byte-identical to an explicitly ISOLATED account.
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
        liquidateCall, placeOrderCall, setLeverageCall, setMarginTypeCall, transferFromPerpCall,
        updateIndexPriceCall, AccountBalanceChanged, Adl, FundingSettled, InsuranceFundChanged,
        InsuranceFundDepleted, Liquidation, PositionChanged,
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

/// Runs `input` as `caller` and asserts it reverted with exactly `reason`, writing nothing.
fn rejects(ctx: &mut TestCtx, caller: Address, input: &[u8], reason: &str) {
    let before = blobs(ctx);
    let out = call(ctx, caller, input);
    assert!(out.reverted, "expected a revert with {reason:?}");
    assert_eq!(revert_reason(&out), reason);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(blobs(ctx), before, "a refused call wrote state");
}

/// Every account and position blob plus the fund, for "a refused call wrote nothing".
fn blobs(ctx: &mut TestCtx) -> Vec<String> {
    let mut out = Vec::new();
    for u in USERS {
        out.push(format!("{:?}", storage::load_account(ctx, u).unwrap()));
        for m in MARKETS {
            out.push(format!("{:?}", pos(ctx, u, m)));
        }
    }
    out.push(format!("IF {}", insurance_fund(ctx)));
    out
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
    let fs = FundingState {
        last_funding_rate: 7_500,
        next_funding_ts: 0,
        cumulative_funding_index: index,
    };
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

// ── The P2 liquidation / ADL interim ─────────────────────────────────────────────

/// An oracle tick on `market` — the mark becomes `median(index, index + basis, last trade)`, i.e.
/// the index here (no basis samples, funding off) — followed by the liquidation sweep.
fn tick(ctx: &mut TestCtx, market: u64, index: u64, ts: u64) -> Vec<Log> {
    let logs = ok(
        ctx,
        ADMIN,
        &updateIndexPriceCall { marketId: market, indexPrice: index, timestamp: ts }.abi_encode(),
    );
    assert_eq!(storage::load_mark_price(ctx, market).unwrap(), index, "the tick set the mark");
    logs
}

fn liquidations(logs: &[Log]) -> Vec<Liquidation> {
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&Liquidation::SIGNATURE_HASH))
        .map(|l| Liquidation::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .collect()
}

fn adl_fills(logs: &[Log]) -> Vec<(Address, u64, u64)> {
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&Adl::SIGNATURE_HASH))
        .map(|l| Adl::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .map(|e| (e.adlUser, e.qty, e.price))
        .collect()
}

/// The sweep and manual `liquidate()` leave a CROSSED position alone — even one the isolated
/// predicate would take on its own slippage, on an account that is genuinely below cross
/// maintenance — with no `Err` (the sweep would panic) and no write, while the SAME sweep still
/// liquidates an isolated position in the same market and ADLs its residual.
#[test]
fn the_sweep_and_liquidate_leave_a_crossed_position_alone() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    set_type(&mut ctx, CAROL, M1, ISOLATED);
    set_leverage(&mut ctx, ALICE, M1, 3);
    set_leverage(&mut ctx, CAROL, M1, 3);
    set_wallet(&mut ctx, ALICE, 400 * USD);
    let ledger = Ledger::open(&mut ctx);

    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 20, GTC));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 10, IOC)); // cross long 10
    ok(&mut ctx, CAROL, &order(M1, BUY, px(100), 10, IOC)); // isolated long 10 at 3x
    ledger.check(&mut ctx, "after the opens");
    let alice_before = (pos(&mut ctx, ALICE, M1), storage::load_account(&mut ctx, ALICE).unwrap());

    // $100 → $70: CAROL's silo is below maintenance; ALICE's ACCOUNT is too (E = W − $300 against
    // Σ MM = $116.67), and her margin-free position would fail the isolated predicate outright.
    let logs = tick(&mut ctx, M1, px(70), 30);
    ledger.check(&mut ctx, "after the sweep");
    let liq = liquidations(&logs);
    assert_eq!(liq.len(), 1, "exactly one liquidation");
    assert_eq!(liq[0].user, CAROL);
    assert_eq!(pos(&mut ctx, CAROL, M1).amount, 0);
    assert_eq!(adl_fills(&logs), vec![(BOB, 10, px(70))], "CAROL's residual went to BOB at mark");
    assert_eq!(
        (pos(&mut ctx, ALICE, M1), storage::load_account(&mut ctx, ALICE).unwrap()),
        alice_before,
        "the cross position and its account are untouched"
    );
    assert!(position_changes(&logs, ALICE).is_empty());
    let f = crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, None).unwrap();
    assert!(f.below_cross_maintenance(), "reported below maintenance (P3 acts on it): {f:?}");

    // Manual liquidation refuses it, cleanly.
    let out = call(&mut ctx, CAROL, &liquidateCall { user: ALICE, marketId: M1 }.abi_encode());
    assert!(out.reverted);
    assert_eq!(revert_reason(&out), crate::risk::LIQUIDATE_CROSSED_REJECT);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(
        (pos(&mut ctx, ALICE, M1), storage::load_account(&mut ctx, ALICE).unwrap()),
        alice_before
    );
    // And the core answers it totally — `Crossed`, never an `Err`.
    let market = storage::load_market(&mut ctx, M1).unwrap().unwrap();
    let (mut budget, mut deferred) = (16, false);
    let outcome = crate::risk::liquidate_position(
        &mut ctx, ALICE, M1, &market, px(70), CAROL, &mut budget, &mut deferred,
    );
    assert_eq!(outcome.unwrap(), crate::risk::LiquidationOutcome::Crossed);
    assert!(!deferred);
}

/// J4: an ISOLATED liquidation close walks no account fold, so a CROSSED leg it does not need
/// cannot fail it. CAROL holds an isolated 3x long of 10 on M1 and a CROSSED long of 1 on M2 whose
/// pending funding no `i64` holds — a leg the CHECKED fold (the gates' and views') refuses with
/// `math: funding payment exceeds i64`. The tick to $70 still liquidates the isolated position into
/// BOB's bid: the close's fee is waived and it moves no silo cash, so its fills gate passes at any
/// balance and nothing reads the cross leg. Folded, the leg's `Err` would reach the sweep's
/// `panic!` on every node. The cross leg is left as it was.
#[test]
fn an_isolated_liquidation_close_never_folds_a_failing_cross_leg() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
    }
    set_type(&mut ctx, CAROL, M1, ISOLATED);
    set_leverage(&mut ctx, CAROL, M1, 3);
    trade(&mut ctx, M2, CAROL, BOB, BUY, px(100), 1); // cross long 1 on M2
    trade(&mut ctx, M1, CAROL, BOB, BUY, px(100), 10); // isolated 3x long 10 on M1
    ok(&mut ctx, BOB, &order(M1, BUY, px(69), 10, GTC)); // the bid the close lands on

    // `−1 · 10^21 / 10^2` of funding on the one cross contract: past `i64`.
    set_funding_index(&mut ctx, M2, 1_000_000_000_000_000_000_000);
    let cross_leg = pos(&mut ctx, CAROL, M2);
    let m2 = storage::load_market(&mut ctx, M2).unwrap().unwrap();
    assert!(crate::margin_view::cross_leg(&m2, &cross_leg).is_err(), "the cross leg fails");
    assert!(
        crate::margin_view::cross_fold_checked(&mut ctx, CAROL, None, None, None).is_err(),
        "the checked fold refuses CAROL's account"
    );

    let logs = tick(&mut ctx, M1, px(70), 30);
    let liq = liquidations(&logs);
    assert_eq!(liq.len(), 1, "exactly one liquidation");
    assert_eq!(liq[0].user, CAROL);
    assert_eq!(pos(&mut ctx, CAROL, M1).amount, 0, "closed in full");
    assert_eq!(pos(&mut ctx, BOB, M1).amount, 0, "into BOB's bid, no ADL");
    assert!(adl_fills(&logs).is_empty());
    assert_eq!(pos(&mut ctx, CAROL, M2), cross_leg, "the cross leg is untouched");
}

/// The ADL scene shared by the two winner tests: CAROL is an ISOLATED 3x long of 10, opened against
/// ALICE (CROSSED, 3x, short 5) and BOB (ISOLATED, 1x, short 5), on an empty book, so a tick to $70
/// liquidates CAROL and her whole position is an ADL residual — solvent, so filled at the mark.
fn adl_scene(ctx: &mut TestCtx, alice_wallet: i64) {
    set_type(ctx, BOB, M1, ISOLATED);
    set_type(ctx, CAROL, M1, ISOLATED);
    set_leverage(ctx, ALICE, M1, 3);
    set_leverage(ctx, CAROL, M1, 3);
    set_wallet(ctx, ALICE, alice_wallet);
    ok(ctx, ALICE, &order(M1, SELL, px(100), 5, GTC));
    ok(ctx, BOB, &order(M1, SELL, px(100), 5, GTC));
    ok(ctx, CAROL, &order(M1, BUY, px(100), 10, IOC));
}

/// A CROSSED winner in the clean tier is RANKED on its ACCOUNT — `uPnL / E` — so a deep cross
/// wallet makes it a LOW-leverage winner: `$150 / ($1M + $150)` ranks it behind BOB's
/// `$150 / ($500 − $0.10 + $150)`. (Read the isolated way its margin-free position would score
/// `uPnL / (uPnL + 0) = 1`, the maximum, and be deleveraged first.) Its fill closes its position at
/// the mark and the realized $150 lands in its WALLET; the isolated winner ahead of it is paid
/// through its silo as before.
#[test]
fn adl_ranks_a_cross_winner_on_its_account_and_settles_it_into_the_wallet() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    adl_scene(&mut ctx, WALLET);
    let ledger = Ledger::open(&mut ctx);
    let w0 = wallet(&mut ctx, ALICE);
    let bob_w0 = wallet(&mut ctx, BOB);
    let bob_silo = pos(&mut ctx, BOB, M1).margin;

    let logs = tick(&mut ctx, M1, px(70), 30);
    ledger.check(&mut ctx, "after the liquidation + ADL");
    crate::events::stream_test_support::assert_account_update_groups(&logs);
    assert_eq!(liquidations(&logs).len(), 1);
    assert_eq!(
        adl_fills(&logs),
        vec![(BOB, 5, px(70)), (ALICE, 5, px(70))],
        "the thinner isolated winner first, the deep cross account second, both at the mark"
    );
    let gain = value(px(100), 5) - value(px(70), 5);
    assert_eq!(wallet(&mut ctx, ALICE), w0 + gain, "realized into the cross wallet");
    let a = pos(&mut ctx, ALICE, M1);
    assert_eq!((a.amount, a.v_quote_balance, a.margin), (0, 0, 0));
    let row = position_changes(&logs, ALICE);
    assert_eq!((row.len(), row[0].realizedPnl, row[0].margin), (1, gain, 0));
    assert_eq!(wallet(&mut ctx, BOB), bob_w0 + bob_silo + gain, "the silo returned with the gain");
    assert_eq!(count_events::<InsuranceFundDepleted>(&logs), 0);
}

/// A CROSSED winner whose ACCOUNT is underwater (`E ≤ 0`, here through a deep loss on its other
/// cross position) is not a clean candidate: it is held back to the fallback tier, the clean
/// isolated winner is filled first, and only the remainder reaches it — under `BothSides`, where
/// its fill still settles into its (negative) wallet with no bad debt of its own. Total throughout:
/// no `Err`, no division by its zero margin.
#[test]
fn adl_holds_an_underwater_cross_winner_back_to_the_fallback_tier() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    adl_scene(&mut ctx, 1_000 * USD);
    // ALICE's other cross leg: a long of 20 on M2, marked down $1,200 — E goes below zero.
    set_type(&mut ctx, BOB, M2, ISOLATED);
    set_leverage(&mut ctx, ALICE, M2, 3);
    trade(&mut ctx, M2, ALICE, BOB, BUY, px(100), 20);
    set_mark(&mut ctx, M2, px(40));
    let ledger = Ledger::open(&mut ctx);
    let w0 = wallet(&mut ctx, ALICE);
    let e = crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, None).unwrap().e;
    assert!(e + 150 * USD as i128 <= 0, "ALICE's account is underwater at $70 too: E = {e}");

    let logs = tick(&mut ctx, M1, px(70), 30);
    ledger.check(&mut ctx, "after the liquidation + ADL");
    assert_eq!(
        adl_fills(&logs),
        vec![(BOB, 5, px(70)), (ALICE, 5, px(70))],
        "the clean isolated winner first, the underwater cross one only as fallback"
    );
    let gain = value(px(100), 5) - value(px(70), 5);
    assert_eq!(wallet(&mut ctx, ALICE), w0 + gain);
    assert_eq!(pos(&mut ctx, ALICE, M1).margin, 0);
}

// ── J6: an isolated silo is funded with wallet cash only ─────────────────────────

/// ALICE holds a CROSSED long deep in profit and a small wallet. Her available balance — which
/// counts that profit — admits an ISOLATED open whose silo is larger than her wallet, and J6 refuses
/// it: the silo would be cross profit turned into isolated collateral, with the wallet carrying the
/// hole. A smaller isolated open, whose silo the wallet holds, goes through and leaves the wallet
/// non-negative.
#[test]
fn j6_a_taker_isolated_silo_is_funded_from_the_wallet_only() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
    }
    set_type(&mut ctx, ALICE, M2, ISOLATED);
    set_leverage(&mut ctx, ALICE, M1, 3);
    set_wallet(&mut ctx, ALICE, 400 * USD);
    let ledger = Ledger::open(&mut ctx);

    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    set_mark(&mut ctx, M1, px(200)); // cross uPnL +$1,000
    let w0 = wallet(&mut ctx, ALICE);
    assert_eq!(w0, 400 * USD - fee(value(px(100), 10), TAKER_PPM));
    let silo = value(px(100), 5); // 1x
    let f = crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, None).unwrap();
    assert!(f.a >= silo as i128, "the available balance alone would admit it: A = {}", f.a);
    assert!(silo > w0, "but the wallet does not hold the silo's cash");

    ok(&mut ctx, BOB, &order(M2, SELL, px(100), 5, GTC));
    rejects(
        &mut ctx,
        ALICE,
        &order(M2, BUY, px(100), 5, IOC),
        crate::trading::ISOLATED_SILO_CASH_REJECT,
    );
    ledger.check(&mut ctx, "after the refusal");

    // A silo the wallet does hold.
    let logs = ok(&mut ctx, ALICE, &order(M2, BUY, px(100), 3, IOC));
    ledger.check(&mut ctx, "after the funded open");
    let n = value(px(100), 3);
    assert_eq!(pos(&mut ctx, ALICE, M2).margin, n - fee(n, TAKER_PPM));
    assert_eq!(wallet(&mut ctx, ALICE), w0 - n);
    assert!(wallet(&mut ctx, ALICE) >= 0);
    assert_eq!(position_changes(&logs, ALICE)[0].marginType, ISOLATED);

    // The batch status table files the refusal under the same cause as any unaffordable order.
    assert_eq!(
        crate::batch::reason_code(&crate::errors::perp_err(
            crate::trading::ISOLATED_SILO_CASH_REJECT
        )),
        crate::batch::PerpBatchReason::InsufficientMargin
    );
}

/// The MAKER side keeps its M1 cap verbatim: an ISOLATED resting order admitted on cross profit
/// fills, and its silo receives the wallet's cash and not a unit more — the wallet ends at zero,
/// never below, and the silo is short (it carries the gap, as it always has).
#[test]
fn j6_a_maker_isolated_silo_stays_capped_at_the_cash_at_hand() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
    }
    set_type(&mut ctx, ALICE, M2, ISOLATED);
    set_leverage(&mut ctx, ALICE, M1, 3);
    set_wallet(&mut ctx, ALICE, 400 * USD);
    let ledger = Ledger::open(&mut ctx);

    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    set_mark(&mut ctx, M1, px(200));
    let w0 = wallet(&mut ctx, ALICE);
    ok(&mut ctx, ALICE, &order(M2, BUY, px(100), 5, GTC)); // admitted on A
    ok(&mut ctx, BOB, &order(M2, SELL, px(100), 5, IOC));
    ledger.check(&mut ctx, "after the maker fill");

    let n = value(px(100), 5);
    assert!(n > w0);
    assert_eq!(wallet(&mut ctx, ALICE), 0, "the silo took the cash at hand and no more");
    assert_eq!(pos(&mut ctx, ALICE, M2).margin, w0 - fee(n, MAKER_PPM as u64));
}

// ── The negative-wallet audit ─────────────────────────────────────────────────────

fn transfer_out(amount: u64) -> Vec<u8> {
    transferFromPerpCall { amount }.abi_encode()
}

fn transfer_in(amount: u64) -> Vec<u8> {
    crate::interface::IPerpDex::transferToPerpCall { amount }.abi_encode()
}

/// The money-out gates read `X = max(0, min(cWB − Σ_I ooIM − Σ_C MM, A))`, not `A`: cross profit
/// admits orders but never leaves the wallet, and a NEGATIVE cross wallet withdraws nothing however
/// much profit stands behind it — while a deposit nets against it.
#[test]
fn a_negative_cross_wallet_and_cross_profit_never_leave_through_the_money_out_gates() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
        set_type(&mut ctx, CAROL, m, ISOLATED);
        set_leverage(&mut ctx, ALICE, m, 3);
    }
    set_wallet(&mut ctx, ALICE, 2_000 * USD);
    let ledger = Ledger::open(&mut ctx);

    // A cross long that triples: +$2,000 of profit on a $2,000 wallet.
    trade(&mut ctx, M2, ALICE, BOB, BUY, px(100), 10);
    set_mark(&mut ctx, M2, px(300));
    let f = crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, None).unwrap();
    let w = wallet(&mut ctx, ALICE);
    assert!(f.a > 2_500 * USD as i128, "A counts the profit: {}", f.a);
    // X is the wallet less the cross maintenance margin ($500 at $3,000 of notional): none of the
    // profit, and none of what maintenance needs.
    assert_eq!(f.x, w as i128 - 500 * USD as i128, "X does not count it");
    assert!(f.x > 0 && f.x < f.a);
    // More than X is refused, X itself goes through.
    rejects(
        &mut ctx,
        ALICE,
        &transfer_out(f.x as u64 + 1),
        "transferFromPerp: insufficient perp wallet balance",
    );
    ok(&mut ctx, ALICE, &transfer_out(f.x as u64));
    ledger.check(&mut ctx, "after withdrawing X");

    // A cross loss on M1 takes the wallet negative: X is 0, so not one unit leaves — not through
    // `transferFromPerp`, not through `internalTransfer` from the perp wallet.
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    set_mark(&mut ctx, M1, px(40));
    ok(&mut ctx, ALICE, &order(M1, SELL, px(40), 10, GTC));
    ok(&mut ctx, CAROL, &order(M1, BUY, px(40), 10, IOC));
    ledger.check(&mut ctx, "after the losing close");
    let w = wallet(&mut ctx, ALICE);
    assert!(w < 0, "negative wallet: {w}");
    let f = crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, None).unwrap();
    assert!(f.a > 0, "the remaining cross profit still admits orders: A = {}", f.a);
    assert_eq!(f.x, 0);
    rejects(&mut ctx, ALICE, &transfer_out(1), "transferFromPerp: insufficient perp wallet balance");
    let internal = crate::interface::IPerpDex::internalTransferCall {
        to: ALICE,
        fromWallet: 1,
        toWallet: 0,
        amount: USD as u64,
    }
    .abi_encode();
    rejects(&mut ctx, ALICE, &internal, "internalTransfer: insufficient perp wallet balance");

    // Money IN nets against the deficit — it lands in the same signed field.
    storage::mutate_account(&mut ctx, ALICE, |a| {
        a.usdc_balance = (USD as u64 * 1_000).to_string().into()
    })
    .unwrap();
    let ledger = Ledger::open(&mut ctx);
    let top_up = 300 * USD as u64;
    ok(&mut ctx, ALICE, &transfer_in(top_up));
    assert_eq!(wallet(&mut ctx, ALICE), w + top_up as i64);
    ledger.check(&mut ctx, "after the deposit");
}

/// The custody identity under a RANDOMISED mixed sequence: three traders — ALICE CROSSED in both
/// markets, BOB and CAROL CROSSED or ISOLATED per market at random — at random leverage on thin
/// wallets, placing passive and aggressive orders on both markets, cancelling, transferring out,
/// taking funding charges and credits, and riding oracle ticks that run the liquidation sweep and
/// ADL. The identity is re-checked after EVERY operation, every withdrawal decision is checked
/// against `X`, and the run must actually reach the cross paths it is about: cross fills, a
/// negative cross wallet, liquidations, ADL fills on a cross winner.
#[test]
fn a_randomised_mixed_sequence_keeps_custody_after_every_step() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        let mut mk = storage::load_market(&mut ctx, m).unwrap().unwrap();
        mk.liquidation_fee_rate_bps = 50; // a live clearance fee: wallet → fund, inside the identity
        mk.price_band_bps = 1_000;
        storage::save_market(&mut ctx, &mk).unwrap();
    }
    storage::save_insurance_fund(&mut ctx, 10_000_000 * USD as u64).unwrap();

    let mut seed: u64 = 0xC205_5AFE_2B1D_0021;
    let mut rng = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let traders = [ALICE, BOB, CAROL];
    // ALICE trades taker-fee-free: an underwater cross account cannot pay a taker fee out of a
    // negative available balance (an isolated one cannot either — a pure reduce's draw IS its fee,
    // gated like any other), so without this its losing closes would only ever happen as a maker.
    // Her OPENS are margin-checked all the same: a cross open's requirement sits inside the
    // post-fill available balance, not in its (zero) draw (`settlement::fills_are_covered`).
    set_taker_fee(&mut ctx, ALICE, 0);
    for &u in &traders {
        set_wallet(&mut ctx, u, 1_500 * USD);
        for m in MARKETS {
            if u != ALICE && rng() % 2 == 0 {
                set_type(&mut ctx, u, m, ISOLATED);
            }
            set_leverage(&mut ctx, u, m, if u == ALICE { 3 } else { 1 + rng() % 3 });
        }
    }
    let ledger = Ledger::open(&mut ctx);

    let mut index = [0i128; 2];
    // The ORACLE index per market, walked on its own (not off the mark, which the book drags), as a
    // sawtooth between $60 and $160 so positions really go under — the two markets in OPPOSITE
    // phase, so ALICE's legs diverge: a cross wallet only goes negative when one leg's profit backs
    // the realized loss of the other (with the account in profit, `X` withdraws down to `Σ MM`).
    let mut oracle = [px(100) as i64; 2];
    let mut target = [px(60) as i64, px(160) as i64];
    let mut ts = 30;
    let mut resting: Vec<Vec<([u8; 32], u64)>> = vec![Vec::new(); 3];
    let (mut cross_fills, mut negative_cross_wallets, mut liqs, mut cross_adl, mut withdrawals) =
        (0u32, 0u32, 0u32, 0u32, 0u32);
    for step in 0..2_000u32 {
        let i = (rng() % 3) as usize;
        let (u, j) = (traders[i], (rng() % 2) as usize);
        let m = MARKETS[j];
        let roll = rng() % 100;
        let logs = if roll < 55 {
            let mark = storage::load_mark_price(&mut ctx, m).unwrap();
            let side = (rng() % 2) as u8;
            let aggressive = rng() % 2 == 0;
            let offset = (1 + rng() % 4) * 100;
            let price = if (side == BUY) == aggressive {
                mark + offset
            } else {
                mark.saturating_sub(offset).max(100)
            };
            let tif = if aggressive { IOC } else { GTC };
            let before = pos(&mut ctx, u, m);
            // ALICE trades bigger: the cross account is the one this test is about.
            let qty = 1 + rng() % if u == ALICE { 15 } else { 8 };
            let out = call(&mut ctx, u, &order(m, side, price, qty, tif));
            let logs = JournalTr::take_logs(ctx.journal_mut());
            if !out.reverted {
                if tif == GTC {
                    resting[i].push((out.bytes[..32].try_into().unwrap(), m));
                }
                let after = pos(&mut ctx, u, m);
                if after.amount != before.amount
                    && crate::margin_type::effective_margin_type(&after) == MarginType::Crossed
                {
                    cross_fills += 1;
                }
            }
            logs
        } else if roll < 65 {
            // Cancel this trader's oldest resting order (it may already have filled — then the
            // cancel is refused, which must write nothing either).
            if resting[i].is_empty() {
                continue;
            }
            let (id, mk) = resting[i].remove(0);
            let input = crate::interface::IPerpDex::cancelOrderCall {
                orderId: id.into(),
                marketId: mk,
            }
            .abi_encode();
            let _ = call(&mut ctx, u, &input);
            JournalTr::take_logs(ctx.journal_mut())
        } else if roll < 71 {
            // A funding epoch on `m`, either sign; every payment is exact on this grid.
            index[j] += 100_000 * ((rng() % 2_001) as i128 - 1_000);
            set_funding_index(&mut ctx, m, index[j]);
            Vec::new()
        } else if roll < 78 {
            let x = crate::margin_view::cross_fold(&mut ctx, u, None, None, None).unwrap().x;
            // ALICE, while exposed, takes everything withdrawable — which leaves her
            // wallet at the cross maintenance margin, so the next realized cross loss takes it
            // below zero.
            let exposed = MARKETS.iter().any(|&mk| pos(&mut ctx, ALICE, mk).amount != 0);
            let amount = if u == ALICE && exposed {
                (x as u64).max(1)
            } else {
                (1 + rng() % 200) * USD as u64
            };
            let out = call(&mut ctx, u, &transfer_out(amount));
            assert_eq!(out.reverted, x < amount as i128, "step {step}: the gate is X = {x}");
            withdrawals += u32::from(!out.reverted);
            JournalTr::take_logs(ctx.journal_mut())
        } else if roll < 80 {
            // An oracle GAP: the mark jumps up to ±40% with no sweep (the next tick runs it). The
            // identity is price-independent, so this only moves value between unrealized terms —
            // and gives the next closes real losses to realize.
            let mark = storage::load_mark_price(&mut ctx, m).unwrap() as i64;
            let jump = mark * ((rng() % 81) as i64 - 40) / 100;
            set_mark(&mut ctx, m, (mark + jump).max(100) as u64);
            Vec::new()
        } else if roll < 84 {
            // Money back in from the spot balance — onto a possibly NEGATIVE wallet, where it nets.
            let spot: U256 =
                storage::load_account(&mut ctx, u).unwrap().usdc_balance.clone().into();
            let amount = spot.to::<u64>().min((1 + rng() % 300) * USD as u64);
            if amount == 0 {
                continue;
            }
            let w = wallet(&mut ctx, u);
            ok(&mut ctx, u, &transfer_in(amount));
            assert_eq!(wallet(&mut ctx, u), w + amount as i64, "step {step}: a top-up nets");
            JournalTr::take_logs(ctx.journal_mut())
        } else {
            if (oracle[j] - target[j]).abs() < 500 {
                target[j] = if target[j] == px(60) as i64 { px(160) as i64 } else { px(60) as i64 };
            }
            oracle[j] =
                (oracle[j] + (target[j] - oracle[j]) / 3 + (rng() % 300) as i64 - 150).max(100);
            ts += 15;
            ok(
                &mut ctx,
                ADMIN,
                &updateIndexPriceCall { marketId: m, indexPrice: oracle[j] as u64, timestamp: ts }
                    .abi_encode(),
            )
        };
        assert_eq!(count_events::<InsuranceFundDepleted>(&logs), 0, "step {step}: socialised loss");
        liqs += count_events::<Liquidation>(&logs) as u32;
        for (winner, _, _) in adl_fills(&logs) {
            if pos(&mut ctx, winner, m).margin_type == CROSSED {
                cross_adl += 1;
            }
        }
        ledger.check(&mut ctx, &format!("step {step}"));
        if wallet(&mut ctx, ALICE) < 0 {
            negative_cross_wallets += 1;
        }
    }
    eprintln!(
        "coverage: cross fills {cross_fills}, steps with ALICE's cross wallet negative \
         {negative_cross_wallets}, liquidations {liqs}, cross ADL fills {cross_adl}, withdrawals \
         {withdrawals}"
    );
    // Coverage floors, about half of what this seed reaches (74 / 55 / 11 / 12 / 89), so a change
    // that quietly stops the run from reaching a path fails here instead of passing vacuously.
    assert!(cross_fills >= 35, "cross fills {cross_fills}");
    assert!(negative_cross_wallets >= 25, "the cross wallet went negative on too few steps");
    assert!(liqs >= 5, "liquidations {liqs}");
    assert!(cross_adl >= 5, "ADL fills on a cross winner {cross_adl}");
    assert!(withdrawals >= 40, "withdrawals {withdrawals}");
}

// ── Gated off: byte identity ─────────────────────────────────────────────────────

/// Every P2b branch keys on the margin type IN FORCE, so with cross margin gated off (production)
/// a position that still STORES the CROSSED default must behave exactly as one explicitly switched
/// to ISOLATED with the gate on: the same call outcomes (revert flag, return bytes, gas), the same
/// event stream byte for byte, and the same stored state (bar the stored type byte itself). The
/// scenario walks every money path this slice touched — taker and maker fills with fees, a flip, a
/// funding settle, the four money-out gates (accepted and refused), a margin top-up, an oracle tick
/// that liquidates and ADLs, and a manual `liquidate()` refusal.
#[test]
fn gated_off_every_money_path_is_byte_identical_to_explicitly_isolated() {
    fn scenario(ctx: &mut TestCtx) -> (Vec<(bool, Vec<u8>, u64)>, Vec<Log>) {
        use crate::interface::IPerpDex::{
            addPositionMarginCall, depositInsuranceFundCall, internalTransferCall,
        };
        let mut outs = Vec::new();
        let mut run = |ctx: &mut TestCtx, who: Address, input: Vec<u8>| {
            let out = call(ctx, who, &input);
            outs.push((out.reverted, out.bytes.to_vec(), out.gas_used));
        };
        run(ctx, CAROL, setLeverageCall { marketId: M2, leverage: 3 }.abi_encode());
        // Fills with fees on both sides, then a flip at a loss.
        run(ctx, BOB, order(M1, SELL, px(100), 10, GTC));
        run(ctx, ALICE, order(M1, BUY, px(100), 10, IOC));
        set_mark(ctx, M1, px(90));
        run(ctx, CAROL, order(M1, BUY, px(90), 15, GTC));
        run(ctx, ALICE, order(M1, SELL, px(90), 15, IOC));
        // A funding epoch, settled at ALICE's and CAROL's next touch.
        set_funding_index(ctx, M1, 10_000 * 2_500);
        run(ctx, ALICE, order(M1, BUY, px(90), 2, GTC));
        run(ctx, CAROL, order(M1, SELL, px(90), 2, IOC));
        // The money-out gates, accepted and refused, and a silo top-up.
        run(ctx, ALICE, transfer_out(100 * USD as u64));
        run(ctx, BOB, transfer_out(WALLET as u64));
        run(
            ctx,
            ALICE,
            internalTransferCall { to: ALICE, fromWallet: 1, toWallet: 0, amount: USD as u64 }
                .abi_encode(),
        );
        run(ctx, ADMIN, depositInsuranceFundCall { amount: 1 }.abi_encode());
        let top_up = addPositionMarginCall { marketId: M1, amount: 10 * USD as u64 };
        run(ctx, ALICE, top_up.abi_encode());
        // CAROL's 3x long on M2 against BOB and ALICE, an empty book, a tick that takes it.
        run(ctx, BOB, order(M2, SELL, px(100), 5, GTC));
        run(ctx, ALICE, order(M2, SELL, px(100), 5, GTC));
        run(ctx, CAROL, order(M2, BUY, px(100), 10, IOC));
        run(
            ctx,
            ADMIN,
            updateIndexPriceCall { marketId: M2, indexPrice: px(70), timestamp: 30 }.abi_encode(),
        );
        run(ctx, CAROL, liquidateCall { user: ALICE, marketId: M2 }.abi_encode());
        (outs, JournalTr::take_logs(ctx.journal_mut()))
    }

    fn state(ctx: &mut TestCtx) -> Vec<String> {
        let mut out = vec![format!("IF {}", insurance_fund(ctx))];
        for u in USERS {
            out.push(format!("{:?}", storage::load_account(ctx, u).unwrap()));
            for m in MARKETS {
                let mut p = pos(ctx, u, m);
                p.margin_type = 0; // the one byte the two runs store differently, on purpose
                out.push(format!("{:?}", p));
            }
        }
        out
    }

    // Gated off — production: every slot still stores the CROSSED default.
    let mut gated = make_ctx();
    let (gated_outs, gated_logs) = scenario(&mut gated);
    let gated_state = state(&mut gated);

    // Gated on, every slot ISOLATED by choice.
    let mut isolated = make_ctx();
    let (isolated_outs, isolated_logs, isolated_state) = {
        let _cross = crate::margin_type::test_gate::enable();
        for u in USERS {
            for m in MARKETS {
                set_type(&mut isolated, u, m, ISOLATED);
            }
        }
        let (outs, logs) = scenario(&mut isolated);
        (outs, logs, state(&mut isolated))
    };

    assert_eq!(gated_outs, isolated_outs, "the same outcomes, return data and gas");
    assert_eq!(gated_logs, isolated_logs, "the same events, byte for byte");
    assert_eq!(gated_state, isolated_state, "the same stored state");
    // The scenario really reached what it claims to.
    assert!(gated_outs.iter().any(|o| o.0), "some refusals");
    assert_eq!(liquidations(&gated_logs).len(), 1);
    assert!(!adl_fills(&gated_logs).is_empty(), "an ADL fill");
    assert!(funding_rows(&gated_logs, ALICE).len() == 1);
}

// ── Cross margin P2c: the gates and controls ──────────────────────────────────────
//
// A child module, so it runs on this file's fixture (`make_ctx`, `Ledger`, `trade`, …) as-is.
#[path = "cross_gates_tests.rs"]
mod gates;
