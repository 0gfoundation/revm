//! **The CROSS golden scenario** — the commitment pin and business snapshot of cross margin as it
//! ships (cross P3, R8), beside `trading::tests::golden`, which pins ISOLATED margin.
//!
//! One block, driven only through the call entry point (`run_perp_dex_call`) from a fresh chain:
//! roles, three markets set up by `addMarket`, real deposits, real orders, oracle ticks at a block
//! clock that moves. Nobody calls `setMarginType` except to go ISOLATED, so every other position
//! is CROSSED by the default. What it reaches, each asserted as it happens:
//!
//! * MIXED accounts: ALICE is CROSSED on M1 and ISOLATED on M2; BOB is ISOLATED everywhere; DAVE,
//!   ERIN and MAKER are CROSSED everywhere.
//! * a FUNDING EPOCH on M1 (the cross long pays into the isolated short's silo), settled into the
//!   cross wallet by the liquidation that touches the position next;
//! * a PARTIAL cross liquidation (Q6): M1 crashes, ALICE's account falls below cross maintenance,
//!   the episode cancels every order she has — her ISOLATED market's included (Q2) — sends a
//!   limit IOC at her bankruptcy price, the book takes part of it, and she is back at maintenance:
//!   the rest stays hers, the flag ends, the clearance fee is 1.25% of the notional closed (Q3);
//! * an ADL (R5): M3 spikes, DAVE's account is BANKRUPT, the book has nothing for his IOC, and the
//!   short is ADL-ed against the winner that ranks first — BOB's isolated long in profit, ahead of
//!   ERIN's cross long at a LOSS (`uPnL · E / N²`);
//! * a STALE market (Q10): DAVE's other cross leg is on M2, whose oracle never published, so the
//!   episode skips it and the flag stays set — and, a leg being unpriced, no deficit sets the ADL
//!   price (P3 review): M3 goes at the MARK, BOB neutral, the whole deficit waits on DAVE's wallet;
//! * the flag ENDED by a reduce-only close: DAVE closes the M2 leg himself, the only kind of order
//!   a flagged account may place (Q11), and the end of that call ends the episode;
//! * a NEGATIVE-wallet clearance (Q8, R4): that close was DAVE's last position and left his wallet
//!   with his whole deficit, so the insurance fund pays it and the wallet ends at 0.
//!
//! Re-pin procedure, as for the isolated golden: run `cross_golden_scenario`, copy the printed
//! `cross golden commitment =` value. A re-pin is legitimate only when [`CrossSnapshot`] still
//! matches, or the business change that moved it is the intended one and is written down here.
//!
//! Business changes since the first pin:
//! * P3 review — a stale leg sets no ADL price: DAVE's M3 short, ADL-ed while his M2 leg is stale,
//!   now fills at the MARK ($180, was his MM-share bankruptcy price $173.57). BOB's wallet
//!   +$64.30 (`accounts[1]`, $4,735.50 → $4,799.80); DAVE's cleared deficit $35.70 → $100.00, so
//!   the insurance fund −$64.30 (`insurance_fund`, $967.80 → $903.50).
use alloy_sol_types::{SolCall, SolError, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, b256, hardfork::SpecId, Address, FixedBytes, Log, B256, U256};

use crate::interface::IPerpDex::{
    addMarketCall, depositCall, depositInsuranceFundCall, getAccountCall, getFundingStateCall,
    getInsuranceFundCall, getMarkPriceCall, getMarketFeeTotalCall, getPositionCall,
    getSymbolConfigCall, initAdminCall, placeOrderCall, setLeverageCall, setMarginTypeCall,
    setOracleAddressCall, setUserFeeRatesCall, transferToPerpCall, updateIndexPriceCall,
    AccountBalanceChanged, Adl, CrossLiquidationEnded, CrossLiquidationStarted, FundingSettled,
    InsuranceFundChanged, Liquidation, OrderCancelled, Trade,
};
use crate::{run_perp_dex_call, storage, PERP_DEX_ADDRESS, USDC_ADDRESS};

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const ORACLE: Address = address!("0000000000000000000000000000000000000c1e");
const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const DAVE: Address = address!("4444444444444444444444444444444444444444");
const ERIN: Address = address!("5555555555555555555555555555555555555555");
const MAKER: Address = address!("7777777777777777777777777777777777777777");
const TRADERS: [Address; 5] = [ALICE, BOB, DAVE, ERIN, MAKER];

/// Three markets on one grid: base 0 decimals, price 2 decimals, so one contract at `P` cents is
/// worth `P · 10^4` quote units. Default tier table (one bracket at 3x: `MM = |N| / 6`), a 15 s
/// price interval (a mark older than 45 s is stale), Binance's 1.25% liquidation fee, the band off.
/// M1 alone runs funding (hourly, at the interest-rate clamp).
const M1: u64 = 1;
const M2: u64 = 2;
const M3: u64 = 3;
const MARKETS: [u64; 3] = [M1, M2, M3];
const USD: u64 = 1_000_000;

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const GTC: u8 = 0;
const IOC: u8 = 1;
const REDUCE_ONLY: u8 = 1;
const CROSSED: u8 = 0;
const ISOLATED: u8 = 1;
/// `OrderCancelled.reason` 3 = Liquidation; `AccountBalanceChanged.reason` 7 = INSURANCE_CLEAR.
const CANCEL_LIQUIDATION: u8 = 3;
const INSURANCE_CLEAR: u8 = 7;

const fn px(dollars: u64) -> u64 {
    dollars * 100
}

/// Pinned final commitment-slot value of [`run_cross_golden_scenario`].
///
/// Re-pinned within 39 for P4 (the cross-leg summary): an account holding a position CROSSED in
/// force now has a per-user "xleg" blob (`types::CrossLegs` — one entry per such position,
/// maintained by `save_position`), written in every block that moves one of its cross positions.
/// A WRITE-SET change only — new keys, no other key's bytes move — and `CrossSnapshot` UNCHANGED.
/// VERIFIED: with the "xleg" keys kept out of the block delta the scenario reproduced the previous
/// pin 0x9c7be213a8addb7411d483e0ab00a49f8e61d90ac205f7be905fdcbfa2d44132 (39, P3 review: a stale
/// leg sets no ADL price).
///
/// Re-pinned within 39 for the P4 review (finding 5): the "xleg" blob's codec is now one msgpack
/// `bin` of packed varint entries instead of a positional msgpack array (`types::CrossLegs`), so
/// every "xleg" value's bytes change and nothing else does — VERIFIED: with the "xleg" keys kept
/// out of the block delta the scenario still reproduces 0x9c7be213…4132. `CrossSnapshot`
/// UNCHANGED. Previous pin 0x3c401b0086975d55a76f24c4bdea8fb637fb5114c93b7ff9cc1a56bedb5173b9.
const CROSS_GOLDEN_COMMITMENT: B256 =
    b256!("0x60470df0b8c7f002d65d94138759b0bba11c791b1446a49ab64e8e2714504b3f");

/// The business end state, read back through the views (and the liquidation flag, which no view
/// reports, from the account record). Pins cross margin's semantics independently of the hash.
#[derive(Debug, PartialEq, Eq)]
struct CrossSnapshot {
    /// Per trader, per market M1 / M2 / M3: `(amount, vQuoteBalance, margin)` (`getPosition`).
    positions: [[(i64, i64, i64); 3]; 5],
    /// Per trader: `getAccount`'s `(totalCrossWalletBalance, crossMarginBalance,
    /// availableBalance, maxWithdrawAmount, totalCrossMaintMargin)`.
    accounts: [(i64, i64, i64, u64, u64); 5],
    /// Per trader: the liquidation flag (`liq_flags` bit 0).
    in_liquidation: [bool; 5],
    insurance_fund: u64,
    /// The admin's perp wallet — the trading-fee sink — after its insurance-fund deposit.
    admin_wallet: i64,
    marks: [u64; 3],
    market_fee_totals: [u64; 3],
    /// M1's `getFundingState`: `(fundingRate, nextFundingTime)`.
    m1_funding: (i64, u64),
}

fn expected_snapshot() -> CrossSnapshot {
    const D: i64 = USD as i64;
    CrossSnapshot {
        positions: [
            // ALICE: 6 of her CROSSED M1 long left by the partial liquidation (no silo); her
            // ISOLATED M2 short untouched, its $200 silo less the $0.10 taker fee.
            [(6, -600 * D, 0), (-2, 200 * D, 199_900_000), (0, 0, 0)],
            // BOB: his ISOLATED M1 short (silo $1,000 less a $0.20 maker fee; his $0.10 of M1
            // funding is still pending); his M3 long ADL-ed away.
            [(-10, 1_000 * D, 999_800_000), (0, 0, 0), (0, 0, 0)],
            // DAVE: flat everywhere — M3 by ADL, M2 by his own reduce-only close.
            [(0, 0, 0); 3],
            // ERIN: CROSSED longs, no silos; M3's at a loss was NOT ADL-ed (it ranked last).
            [(0, 0, 0), (2, -200 * D, 0), (6, -1_140 * D, 0)],
            // MAKER: the 4 it bought from ALICE's liquidation, flat on M2, short 6 on M3.
            [(4, -280 * D, 0), (0, 0, 0), (-6, 1_140 * D, 0)],
        ],
        accounts: [
            // ALICE: W = $399.50 − $0.10 funding − $120 realized − $3.50 fee = $275.90;
            // E = W − $180 = $95.90; A = E − PIM $140 = −$44.10; X = 0; Σ MM = $70.
            (275_900_000, 95_900_000, -44_100_000, 0, 70 * USD),
            // BOB, isolated only: W = $5,000 − $2,000 silos + $999.80 + $800 from the ADL (his
            // $400 silo and the $400 gain at the $180 mark).
            (4_799_800_000, 4_799_800_000, 4_799_800_000, 4_799_800_000, 0),
            // DAVE: the −$100 cleared to exactly 0.
            (0, 0, 0, 0, 0),
            // ERIN: E = $1,000 − $60 (M3 at $180 vs $190); A = E − PIM ($200 + $360);
            // X = A; Σ MM = $33.33 + $180.
            (1_000 * D, 940 * D, 380 * D, 380 * USD, 213_333_333),
            // MAKER: E = $100,000 + $60 (M3); A = E − PIM ($280 + $1,080).
            (100_000 * D, 100_060 * D, 98_700 * D, 98_700 * USD, 226_666_666),
        ],
        in_liquidation: [false; 5],
        // $1,000 deposited + ALICE's $3.50 clearance fee − DAVE's $100 clearance.
        insurance_fund: 903_500_000,
        // $2,000 − the $1,000 fund deposit + $1.00 of trading fees.
        admin_wallet: 1_001_000_000,
        // M2's is `addMarket`'s initial mark: its oracle never published.
        marks: [px(70), px(100), px(180)],
        market_fee_totals: [700_000, 100_000, 200_000],
        // The interest-rate clamp; the epoch that started at t = 900 rolled over at 4500.
        m1_funding: (100, 8_100),
    }
}

// ── Plumbing ───────────────────────────────────────────────────────────────────────────────

fn fresh_chain() -> TestCtx {
    let mut db = InMemoryDB::default();
    for (user, dollars) in
        [(ALICE, 1_000), (BOB, 5_000), (DAVE, 700), (ERIN, 1_000), (MAKER, 100_000), (ADMIN, 2_000)]
    {
        db.insert_account_storage(
            USDC_ADDRESS,
            storage::keys::erc20_balance_slot(user).into(),
            U256::from(dollars * USD),
        )
        .unwrap();
    }
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for a in [USDC_ADDRESS, PERP_DEX_ADDRESS, ADMIN, ORACLE].into_iter().chain(TRADERS) {
        JournalTr::load_account(ctx.journal_mut(), a).unwrap();
    }
    ctx
}

fn at(ctx: &mut TestCtx, t: u64) {
    ctx.block.timestamp = U256::from(t);
}

/// A call that must succeed; returns its logs.
#[track_caller]
fn run(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> Vec<Log> {
    let out = run_perp_dex_call(input, 30_000_000, caller, U256::ZERO, false, ctx)
        .expect("a perp call must not hard-fail");
    assert!(
        !out.reverted,
        "call reverted: {:?}",
        alloy_sol_types::Revert::abi_decode(&out.bytes).map(|r| r.reason)
    );
    JournalTr::take_logs(ctx.journal_mut())
}

/// A call whose refusal is the point: it reverts with `expect` in its reason. Run under a
/// checkpoint rolled back afterwards, as the EVM frame of a reverted call is.
#[track_caller]
fn refused(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) {
    let cp = ctx.journal_mut().checkpoint();
    let out = run_perp_dex_call(input, 30_000_000, caller, U256::ZERO, false, ctx)
        .expect("a perp call must not hard-fail");
    assert!(out.reverted, "expected a refusal containing {expect:?}");
    let reason = alloy_sol_types::Revert::abi_decode(&out.bytes).unwrap().reason;
    assert!(reason.contains(expect), "{reason:?} does not contain {expect:?}");
    ctx.journal_mut().checkpoint_revert(cp);
}

fn view<C: SolCall>(ctx: &mut TestCtx, call: &C) -> C::Return {
    let out = run_perp_dex_call(&call.abi_encode(), 30_000_000, ADMIN, U256::ZERO, true, ctx)
        .expect("a view must not hard-fail");
    assert!(!out.reverted, "view reverted");
    C::abi_decode_returns(&out.bytes).unwrap()
}

fn order(market: u64, side: u8, price: u64, qty: u64, tif: u8, flags: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: market,
        side,
        price,
        quantity: qty,
        orderType: LIMIT,
        tif,
        clientOrderId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

#[track_caller]
fn tick(ctx: &mut TestCtx, market: u64, dollars: u64, t: u64) -> Vec<Log> {
    at(ctx, t);
    let input = updateIndexPriceCall { marketId: market, indexPrice: px(dollars), timestamp: t };
    run(ctx, ORACLE, &input.abi_encode())
}

fn evs<E: SolEvent>(logs: &[Log]) -> Vec<E> {
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|l| E::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .collect()
}

fn flagged(ctx: &mut TestCtx, user: Address) -> bool {
    storage::load_account(ctx, user).unwrap().in_liquidation()
}

// ── The scenario ───────────────────────────────────────────────────────────────────────────

fn run_cross_golden_scenario() -> (B256, CrossSnapshot) {
    let mut ctx = fresh_chain();
    let ctx = &mut ctx;

    // Phase 0 (t = 900) — roles, the three markets, fee rates.
    at(ctx, 900);
    run(ctx, ADMIN, &initAdminCall { admin: ADMIN }.abi_encode());
    run(ctx, ADMIN, &setOracleAddressCall { oracle: ORACLE }.abi_encode());
    for m in MARKETS {
        let add = addMarketCall {
            marketId: m,
            baseDecimals: 0,
            priceDecimals: 2,
            tickSize: 1,
            stepSize: 1,
            minQuantity: 1,
            maxQuantity: 1_000_000,
            maxPrice: px(1_000_000),
            priceUpdateInterval: 15,
            fundingInterval: if m == M1 { 3_600 } else { 0 },
            interestRate: 100,
            liquidationFeeRateBps: 125,
            initialMarkPrice: px(100),
            priceBandBps: 1_000_000,
        };
        run(ctx, ADMIN, &add.abi_encode());
    }
    for user in [ALICE, BOB] {
        let rates = setUserFeeRatesCall { user, makerFeePpm: 200, takerFeePpm: 500 };
        run(ctx, ADMIN, &rates.abi_encode());
    }

    // Phase 1 — money in: ERC-20 → spot → perp wallet; the admin seeds the insurance fund.
    for (user, dollars) in
        [(ALICE, 600), (BOB, 5_000), (DAVE, 700), (ERIN, 1_000), (MAKER, 100_000), (ADMIN, 2_000)]
    {
        let spot = if user == ALICE { 1_000 } else { dollars };
        run(ctx, user, &depositCall { amount: U256::from(spot * USD) }.abi_encode());
        run(ctx, user, &transferToPerpCall { amount: dollars * USD }.abi_encode());
    }
    run(ctx, ADMIN, &depositInsuranceFundCall { amount: 1_000 * USD }.abi_encode());

    // Phase 2 — the oracle's first ticks on M1 and M3 (M1's funding epoch starts here). M2's
    // oracle never publishes: its mark is `addMarket`'s initial one, and from 45 s on it is stale.
    tick(ctx, M1, 100, 900);
    tick(ctx, M3, 100, 900);

    // Phase 3 — margin types and leverage. The default is CROSSED; only these choose ISOLATED.
    for m in MARKETS {
        run(ctx, BOB, &setMarginTypeCall { marketId: m, marginType: ISOLATED }.abi_encode());
    }
    run(ctx, ALICE, &setMarginTypeCall { marketId: M2, marginType: ISOLATED }.abi_encode());
    for (user, m) in [(ALICE, M1), (DAVE, M2), (DAVE, M3), (ERIN, M3)] {
        run(ctx, user, &setLeverageCall { marketId: m, leverage: 3 }.abi_encode());
    }
    for user in TRADERS {
        for m in MARKETS {
            let want = if user == BOB || (user == ALICE && m == M2) { ISOLATED } else { CROSSED };
            let got = view(ctx, &getSymbolConfigCall { user, marketId: m }).marginType;
            assert_eq!(got, want, "{user} market {m}: CROSSED unless switched");
        }
    }

    // Phase 4 — positions, every book emptied again as it is filled (a two-sided book would feed
    // the mark's basis window, which this scenario keeps out of its marks).
    //   M1: ALICE CROSSED long 10 @ $100 (3x) against BOB's ISOLATED short.
    run(ctx, BOB, &order(M1, SELL, px(100), 10, GTC, 0));
    run(ctx, ALICE, &order(M1, BUY, px(100), 10, IOC, 0));
    //   M2: DAVE CROSSED long 10 @ $100 against MAKER; ALICE ISOLATED short 2 @ $100 against ERIN.
    run(ctx, MAKER, &order(M2, SELL, px(100), 10, GTC, 0));
    run(ctx, DAVE, &order(M2, BUY, px(100), 10, IOC, 0));
    run(ctx, ERIN, &order(M2, BUY, px(100), 2, GTC, 0));
    run(ctx, ALICE, &order(M2, SELL, px(100), 2, IOC, 0));
    //   M3: DAVE CROSSED short 10 @ $100 against BOB's ISOLATED long; ERIN CROSSED long 6 @ $190.
    run(ctx, BOB, &order(M3, BUY, px(100), 10, GTC, 0));
    run(ctx, DAVE, &order(M3, SELL, px(100), 10, IOC, 0));
    run(ctx, MAKER, &order(M3, SELL, px(190), 6, GTC, 0));
    run(ctx, ERIN, &order(M3, BUY, px(190), 6, IOC, 0));
    //   Resting: ALICE bids on M1 and on her ISOLATED M2 (a closing bid against her short);
    //   MAKER bids 4 @ $70 on M1.
    run(ctx, ALICE, &order(M1, BUY, px(50), 1, GTC, 0));
    run(ctx, ALICE, &order(M2, BUY, px(60), 1, GTC, 0));
    run(ctx, MAKER, &order(M1, BUY, px(70), 4, GTC, 0));

    // Phase 5 — M1's funding epoch: a mid-epoch tick, then the boundary (t = 4500). The index
    // steps; nobody's funding is settled yet — it is lazy, paid at the position's next touch.
    tick(ctx, M1, 100, 1_800);
    tick(ctx, M3, 100, 1_800);
    tick(ctx, M1, 100, 4_500);
    tick(ctx, M3, 100, 4_500);
    assert_eq!(view(ctx, &getFundingStateCall { marketId: M1 }).nextFundingTime, 8_100);

    // Phase 6 (t = 4515) — M1 crashes to $70: ALICE's PARTIAL cross liquidation. Her wallet is
    // $399.50 ($600 − her $200 M2 silo − her $0.50 M1 taker fee; the M2 fee came out of the silo)
    // with $0.10 of M1 funding pending, so `E = cWB + uPnL = $399.40 − $300 = $99.40` against
    // `Σ MM = $700 · 10 / 6 = $116.67`.
    let logs = tick(ctx, M1, 70, 4_515);
    let started = evs::<CrossLiquidationStarted>(&logs);
    assert_eq!(started.len(), 1);
    assert_eq!(
        (started[0].user, started[0].triggerMarketId, started[0].crossMarginBalance),
        (ALICE, M1, 99_400_000)
    );
    assert_eq!(started[0].totalCrossMaintMargin, 116_666_666);
    // Every order she has is cancelled, her ISOLATED market's included (Q2).
    let cancelled = evs::<OrderCancelled>(&logs);
    assert_eq!(
        cancelled.iter().map(|c| (c.user, c.marketId, c.reason)).collect::<Vec<_>>(),
        vec![(ALICE, M1, CANCEL_LIQUIDATION), (ALICE, M2, CANCEL_LIQUIDATION)]
    );
    // Her M1 funding is settled into the cross wallet before M1 closes.
    let funding = evs::<FundingSettled>(&logs);
    assert_eq!(funding.len(), 1);
    assert_eq!((funding[0].user, funding[0].marketId, funding[0].amount), (ALICE, M1, -100_000));
    // The IOC at her bankruptcy price ($60.06) takes MAKER's 4 @ $70, and that is enough:
    // `E = $95.90 ≥ Σ MM = $70` — after the 1.25% fee on the $280 closed ($3.50). The other 6
    // stay hers, nothing is ADL-ed, the flag ends.
    let trades = evs::<Trade>(&logs);
    assert_eq!(trades.len(), 1);
    assert_eq!(
        (trades[0].taker, trades[0].maker, trades[0].quantity, trades[0].price),
        (ALICE, MAKER, 4, px(70))
    );
    assert!(evs::<Adl>(&logs).is_empty(), "stopped before ADL");
    let liq = evs::<Liquidation>(&logs);
    assert_eq!(liq.len(), 1);
    assert_eq!(
        (liq[0].user, liq[0].marketId, liq[0].amount, liq[0].reward, liq[0].marginType),
        (ALICE, M1, 10, 3_500_000, CROSSED)
    );
    let ended = evs::<CrossLiquidationEnded>(&logs);
    assert_eq!(ended.len(), 1);
    assert_eq!(
        (ended[0].user, ended[0].marketId, ended[0].crossMarginBalance),
        (ALICE, M1, 95_900_000)
    );
    assert_eq!(ended[0].totalCrossMaintMargin, 70 * USD);
    assert!(!flagged(ctx, ALICE));
    assert_eq!(view(ctx, &getPositionCall { user: ALICE, marketId: M1 }).amount, 6);

    // Phase 7 (t = 4530) — M3 spikes to $180: DAVE is BANKRUPT. `E = $700 − $800 = −$100`
    // against `Σ MM = $300 (M3) + $166.67 (M2)`. M3, the trigger, closes first: no ask for the
    // IOC, so the short is ADL-ed. M2 is stale (its oracle never published), so it is skipped and
    // the flag stays set — and with a leg unpriced no deficit sets the ADL price (P3 review): the
    // 10 go at the MARK, $180 (R5's MM-share price would have been $173.57, charging BOB M3's
    // 300/466.67 share of a deficit measured partly at M2's unpublished mark). BOB's isolated long
    // in profit ranks ahead of ERIN's cross long at a loss and takes all 10. DAVE's wallet is
    // −$100: the whole deficit waits on the account.
    let logs = tick(ctx, M3, 180, 4_530);
    let started = evs::<CrossLiquidationStarted>(&logs);
    assert_eq!(started.len(), 1);
    assert_eq!(
        (started[0].user, started[0].triggerMarketId, started[0].crossMarginBalance),
        (DAVE, M3, -100_000_000)
    );
    assert_eq!(started[0].totalCrossMaintMargin, 466_666_666);
    assert!(evs::<Trade>(&logs).is_empty(), "the book has nothing for the IOC");
    let adl = evs::<Adl>(&logs);
    assert_eq!(
        adl.iter()
            .map(|a| (a.liquidatedUser, a.adlUser, a.marketId, a.qty, a.price))
            .collect::<Vec<_>>(),
        vec![(DAVE, BOB, M3, 10, px(180))]
    );
    let liq = evs::<Liquidation>(&logs);
    assert_eq!(liq.len(), 1);
    assert_eq!(
        (liq[0].user, liq[0].marketId, liq[0].amount, liq[0].reward, liq[0].marginType),
        (DAVE, M3, -10, 0, CROSSED),
        "bankrupt: no clearance fee"
    );
    assert!(evs::<CrossLiquidationEnded>(&logs).is_empty(), "the stale M2 leg is still open");
    assert!(flagged(ctx, DAVE));
    assert_eq!(view(ctx, &getPositionCall { user: DAVE, marketId: M2 }).amount, 10);
    assert_eq!(view(ctx, &getPositionCall { user: ERIN, marketId: M3 }).amount, 6);

    // Phase 8 (t = 4545) — flagged, DAVE may only reduce (Q11): a plain order is `-2023`; his
    // reduce-only sell closes the M2 leg into MAKER's bid at $100. That was his LAST position:
    // the end of the call ends the episode and clears the −$100 from the insurance fund (Q8).
    at(ctx, 4_545);
    refused(ctx, DAVE, &order(M2, SELL, px(100), 1, IOC, 0), "-2023 User in liquidation mode now.");
    run(ctx, MAKER, &order(M2, BUY, px(100), 10, GTC, 0));
    let logs = run(ctx, DAVE, &order(M2, SELL, px(100), 10, IOC, REDUCE_ONLY));
    let ended = evs::<CrossLiquidationEnded>(&logs);
    assert_eq!(ended.len(), 1);
    assert_eq!(
        (ended[0].user, ended[0].marketId, ended[0].crossMarginBalance),
        (DAVE, M2, -100_000_000)
    );
    assert_eq!(ended[0].totalCrossMaintMargin, 0, "no cross position left");
    let fund = evs::<InsuranceFundChanged>(&logs);
    assert_eq!(fund.iter().map(|f| f.delta).collect::<Vec<_>>(), vec![-100_000_000]);
    let rows: Vec<(u8, i64)> = evs::<AccountBalanceChanged>(&logs)
        .into_iter()
        .filter(|r| r.user == DAVE)
        .map(|r| (r.reason, r.totalCrossWalletBalance))
        .collect();
    assert_eq!(rows.last(), Some(&(INSURANCE_CLEAR, 0)), "the clearance is his last row");
    assert!(rows.contains(&(0, -100_000_000)), "the deficit was published first: {rows:?}");
    assert!(!flagged(ctx, DAVE));

    let snapshot = take_snapshot(ctx);
    let delta = ctx.journal_mut().take_perp_delta();
    storage::finalize_block_commitment(ctx, &delta).unwrap();
    let commitment = ctx
        .journal_mut()
        .sload(PERP_DEX_ADDRESS, storage::keys::commitment_slot().into())
        .unwrap()
        .data;
    (B256::from(commitment.to_be_bytes::<32>()), snapshot)
}

fn take_snapshot(ctx: &mut TestCtx) -> CrossSnapshot {
    let mut positions = [[(0, 0, 0); 3]; 5];
    let mut accounts = [(0, 0, 0, 0, 0); 5];
    let mut in_liquidation = [false; 5];
    for (i, user) in TRADERS.into_iter().enumerate() {
        for (j, m) in MARKETS.into_iter().enumerate() {
            let p = view(ctx, &getPositionCall { user, marketId: m });
            positions[i][j] = (p.amount, p.vQuoteBalance, p.margin);
        }
        let a = view(ctx, &getAccountCall { user });
        accounts[i] = (
            a.totalCrossWalletBalance,
            a.crossMarginBalance,
            a.availableBalance,
            a.maxWithdrawAmount,
            a.totalCrossMaintMargin,
        );
        in_liquidation[i] = flagged(ctx, user);
    }
    let funding = view(ctx, &getFundingStateCall { marketId: M1 });
    CrossSnapshot {
        positions,
        accounts,
        in_liquidation,
        insurance_fund: view(ctx, &getInsuranceFundCall {}),
        admin_wallet: view(ctx, &getAccountCall { user: ADMIN }).totalCrossWalletBalance,
        marks: MARKETS.map(|m| view(ctx, &getMarkPriceCall { marketId: m })),
        market_fee_totals: MARKETS.map(|m| view(ctx, &getMarketFeeTotalCall { marketId: m })),
        m1_funding: (funding.fundingRate, funding.nextFundingTime),
    }
}

#[test]
fn cross_golden_deterministic() {
    let first = run_cross_golden_scenario();
    let second = run_cross_golden_scenario();
    assert_eq!(first.0, second.0, "the commitment must be deterministic");
    assert_eq!(first.1, second.1, "the business end state must be deterministic");
}

#[test]
fn cross_golden_scenario() {
    let (commitment, snapshot) = run_cross_golden_scenario();
    println!("cross golden commitment = {commitment}");
    println!("cross golden snapshot = {snapshot:#?}");
    assert_eq!(commitment, CROSS_GOLDEN_COMMITMENT, "the cross golden commitment drifted");
    assert_eq!(snapshot, expected_snapshot(), "the cross business end state drifted");
}
