//! Tests for the derived margin read layer.
//!
//! Every rounding mode the reference docs settled with a discriminating mainnet sample gets a
//! test that would FAIL under the refuted alternative — that is the whole point of pinning them:
//! `notional` truncates, `unrealizedProfit` truncates TOWARD ZERO (not floor), `IM`/`PIM`
//! ROUND UP (not floor, not truncate), and `ooIM` is a difference of two round-ups (not one
//! round-up of a difference, which is off by 1 ulp).
//!
//! `divergence_flip_on_the_book_binance_ooim_vs_our_escrow` is the measurement this layer was
//! built for; it is deliberately a first-class test rather than an incidental assertion.

use super::*;
// Imported explicitly rather than leaning on `use super::*`: the parent's import of this is
// `#[cfg(debug_assertions)]`-gated (it feeds a debug-only oracle), so inheriting it made this file
// — and therefore the whole test binary, INCLUDING the golden-commitment test — fail to compile
// under `--release`. Owning the import here is what lets the commitment be checked in both
// profiles.
use crate::math::sum_side_totals;
use alloy_sol_types::SolCall;
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, FixedBytes, U256};

use crate::{
    host::PerpHost,
    interface::IPerpDex::{
        getAccountCall, getAccountMarginReturn, getAccountReturn, getMarginInfoReturn,
        placeOrderCall,
    },
    run_perp_dex_call, storage,
    types::{MarginTiers, Market, OrderEntry, PerpPosition, UserAccount, MAX_USER_MARKETS},
    PERP_DEX_ADDRESS, USDC_ADDRESS,
};

// ── Fixtures ───────────────────────────────────────────────────────────────

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

/// Quote units in one dollar (`QUOTE_DECIMALS = 6`).
const USD: u64 = 1_000_000;

/// Market A — `base_decimals = 0`, `price_decimals = 2`. Prices are cents, quantities are whole
/// base units, and `calc_value(price, qty) = price * qty * 1e4`, i.e. always integral. Used for
/// every test where the arithmetic should be exact so a rounding assertion is unambiguous.
const MARKET_A: u64 = 1;
const A_BASE_DECIMALS: u32 = 0;
const A_PRICE_DECIMALS: u32 = 2;
/// $100.00 in market A's 2-decimal price units.
const P100: u64 = 10_000;
/// $60.00.
const P60: u64 = 6_000;

/// Market B — `base_decimals = 0`, `price_decimals = 8`, so `calc_value(price, qty) =
/// price * qty / 100`: a FRACTIONAL grid. This is the market that can separate
/// truncate-toward-zero from floor, which needs a non-integral quotient on a NEGATIVE value.
const MARKET_B: u64 = 2;
const B_BASE_DECIMALS: u32 = 0;
const B_PRICE_DECIMALS: u32 = 8;

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, BOB, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    ctx
}

fn add_market(
    ctx: &mut TestCtx,
    market_id: u64,
    base_decimals: u32,
    price_decimals: u32,
    mark_price: u64,
) {
    storage::save_market(
        ctx,
        &Market {
            market_id,
            base_decimals,
            price_decimals,
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
            price_band_bps: 0,
            mark_price,
            tiers: MarginTiers::default(),
        },
    )
    .unwrap();
}

/// Market A at mark $100 — the default fixture for the exact-arithmetic tests.
fn setup_a(ctx: &mut TestCtx) {
    add_market(ctx, MARKET_A, A_BASE_DECIMALS, A_PRICE_DECIMALS, P100);
}

fn fund(ctx: &mut TestCtx, user: Address, amount: u64) {
    let mut acc = storage::load_account(ctx, user).unwrap();
    acc.credit_perp(amount).unwrap();
    storage::save_account(ctx, user, acc).unwrap();
}

/// Install a synthetic position. Used where the test only cares about the derived READ; the
/// tests that assert what the ENGINE did drive it through the real call path instead.
fn set_position(
    ctx: &mut TestCtx,
    user: Address,
    market_id: u64,
    amount: i64,
    v_quote_balance: i64,
    margin: i64,
    leverage: u64,
) {
    storage::save_position(
        ctx,
        user,
        market_id,
        &PerpPosition {
            amount,
            v_quote_balance,
            margin,
            leverage,
            ..PerpPosition::default()
        },
    )
    .unwrap();
}

/// A resting entry at `price`. `assuming_price` is left at the BUY rule (== the limit price);
/// [`set_orders`] re-freezes it for whichever side the entry is installed on, so a caller does not
/// have to know `T`.
fn entry(order_id: u8, price: u64, amount: u64) -> OrderEntry {
    let mut id = [0u8; 32];
    id[31] = order_id;
    OrderEntry {
        order_id: id,
        price,
        amount,
        maker_fee_bps: 0,
        assuming_price: price,
    }
}

/// Install resting order lists AND the maintained per-side aggregates exactly the way the engine
/// does, so the state a test reads back is the state a real place/cancel sequence would have
/// left. `buys` must be price-DESC and `sells` price-ASC, the engine's own list invariant.
///
/// This includes FREEZING each entry's Assuming Price the way `rest_in_book` would have: the limit
/// price on a buy, `max(T, limit)` on a sell with `T` resolved from the market's CURRENT mark and
/// last-traded price. A test that then moves the mark is therefore looking at genuinely frozen
/// entries, not at values the read path re-derives.
fn set_orders(
    ctx: &mut TestCtx,
    user: Address,
    market_id: u64,
    buys: &[OrderEntry],
    sells: &[OrderEntry],
) {
    assert!(
        buys.windows(2).all(|w| w[0].price >= w[1].price),
        "buy entries must be price-descending"
    );
    assert!(
        sells.windows(2).all(|w| w[0].price <= w[1].price),
        "sell entries must be price-ascending"
    );
    let market = storage::load_market_ref(ctx, market_id).unwrap().unwrap();
    let (bd, pd) = (market.base_decimals, market.price_decimals);
    let floor = crate::margin_view::assuming_price_floor(ctx, market_id, &market).unwrap();

    let buys: Vec<OrderEntry> = buys
        .iter()
        .map(|e| OrderEntry {
            assuming_price: e.price,
            ..*e
        })
        .collect();
    let sells: Vec<OrderEntry> = sells
        .iter()
        .map(|e| OrderEntry {
            assuming_price: e.price.max(floor),
            ..*e
        })
        .collect();

    storage::save_buy_orders(ctx, user, market_id, &buys.iter().copied().collect()).unwrap();
    storage::save_sell_orders(ctx, user, market_id, &sells.iter().copied().collect()).unwrap();

    let mut pos = storage::load_position(ctx, user, market_id).unwrap();
    let (tbq, tbn) = sum_side_totals(buys.iter().copied(), bd, pd).unwrap();
    let (tsq, tsn) = sum_side_totals(sells.iter().copied(), bd, pd).unwrap();
    pos.total_buy_qty = tbq;
    pos.total_buy_notional = tbn;
    pos.total_sell_qty = tsq;
    pos.total_sell_notional = tsn;
    storage::save_position(ctx, user, market_id, &pos).unwrap();
}

/// Read `getMarginInfo` through the FULL call shell in a STATIC context — this is also the
/// assertion that the selector is registered with `can_be_static = true`.
fn margin_info(ctx: &mut TestCtx, user: Address, market_id: u64) -> getMarginInfoReturn {
    let input = getMarginInfoCall {
        user,
        marketId: market_id,
    }
    .abi_encode();
    let out = run_perp_dex_call(&input, 1_000_000, user, U256::ZERO, true, ctx).unwrap();
    assert!(!out.reverted, "getMarginInfo reverted: {:?}", out.bytes);
    getMarginInfoCall::abi_decode_returns(&out.bytes).unwrap()
}

/// Read `getAccountMargin` through the full call shell in a static context.
fn account_margin(ctx: &mut TestCtx, user: Address, market_ids: &[u64]) -> getAccountMarginReturn {
    let input = getAccountMarginCall {
        user,
        marketIds: market_ids.to_vec(),
    }
    .abi_encode();
    let out = run_perp_dex_call(&input, 1_000_000, user, U256::ZERO, true, ctx).unwrap();
    assert!(!out.reverted, "getAccountMargin reverted: {:?}", out.bytes);
    getAccountMarginCall::abi_decode_returns(&out.bytes).unwrap()
}

/// Read `getAccount` through the FULL call shell in a STATIC context, asserting on the way that
/// the selector is registered `can_be_static = true` and that its FLAT gas is the 20_000 the
/// index-driven roll-up was re-priced to (it walks up to MAX_USER_MARKETS markets, so it sits in
/// the `getMarginInfo` / `getOpenOrders` tier, not the 5_000 scalar-getter tier it used to).
fn get_account(ctx: &mut TestCtx, user: Address) -> getAccountReturn {
    let input = getAccountCall { user }.abi_encode();
    let out = run_perp_dex_call(&input, 1_000_000, user, U256::ZERO, true, ctx).unwrap();
    assert!(!out.reverted, "getAccount reverted: {:?}", out.bytes);
    assert_eq!(
        out.gas_used, 20_000,
        "getAccount's gas must stay FLAT per selector — never per-item or dynamic"
    );
    getAccountCall::abi_decode_returns(&out.bytes).unwrap()
}

/// `getAccountMarginReturn` as a comparable tuple (the `sol!`-generated struct derives neither
/// `PartialEq` nor `Debug`).
fn as_tuple(a: &getAccountMarginReturn) -> (i64, i64, u64, u64, u64, u64, i64, i64) {
    (
        a.totalCrossWalletBalance,
        a.crossMarginBalance,
        a.totalInitialMargin,
        a.totalPositionInitialMargin,
        a.totalOpenOrderInitialMargin,
        a.totalMaintMargin,
        a.totalUnrealizedProfit,
        a.availableBalance,
    )
}

/// The revert reason string of a call that is expected to revert.
fn revert_reason(ctx: &mut TestCtx, input: &[u8]) -> String {
    let out = run_perp_dex_call(input, 1_000_000, ALICE, U256::ZERO, true, ctx).unwrap();
    assert!(out.reverted, "expected a revert");
    // Error(string): selector(4) | offset(32) | len(32) | data
    let len = u64::from_be_bytes(out.bytes[60..68].try_into().unwrap()) as usize;
    String::from_utf8(out.bytes[68..68 + len].to_vec()).unwrap()
}

/// Place a real order through the engine (so the book, the aggregates and the gate are
/// the engine's own, not the test's).
fn place(ctx: &mut TestCtx, caller: Address, side: u8, price: u64, qty: u64) {
    let input = placeOrderCall {
        marketId: MARKET_A,
        side,
        price,
        quantity: qty,
        orderType: 0, // Limit
        tif: 0,       // GTC
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    crate::trading::run_place_order(&input, caller, ctx).unwrap();
}

// ── 1. Flat account ────────────────────────────────────────────────────────

#[test]
fn flat_account_reads_back_zero_and_sane() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    fund(&mut ctx, ALICE, 500 * USD);

    let i = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(i.markPrice, P100);
    assert_eq!(i.positionAmt, 0);
    assert_eq!(i.vQuoteBalance, 0);
    // Sane, not zero: a defaulted position reports leverage 1, never 0 (nothing divides by it).
    assert_eq!(i.leverage, 1);
    assert_eq!((i.bidNotional, i.askNotional), (0, 0));
    assert_eq!(i.notional, 0);
    assert_eq!(i.unrealizedProfit, 0);
    assert_eq!(i.isolatedMargin, 0);
    assert_eq!(i.positionInitialMargin, 0);
    assert_eq!(i.openOrderInitialMargin, 0);
    assert_eq!(i.initialMargin, 0);
    assert_eq!(i.maintMargin, 0);
    assert_eq!(i.positionMargin, 0);

    let a = account_margin(&mut ctx, ALICE, &[MARKET_A]);
    assert_eq!(a.totalCrossWalletBalance, (500 * USD) as i64);
    assert_eq!(a.crossMarginBalance, (500 * USD) as i64);
    assert_eq!(a.totalInitialMargin, 0);
    assert_eq!(a.totalPositionInitialMargin, 0);
    assert_eq!(a.totalOpenOrderInitialMargin, 0);
    assert_eq!(a.totalMaintMargin, 0);
    assert_eq!(a.totalUnrealizedProfit, 0);
    // No resting orders ⇒ Σ ooIM is 0 ⇒ available == wallet.
    assert_eq!(a.availableBalance, (500 * USD) as i64);

    // An empty market list is legal and yields the pure ledger view.
    let empty = account_margin(&mut ctx, ALICE, &[]);
    assert_eq!(empty.totalCrossWalletBalance, (500 * USD) as i64);
    assert_eq!(empty.availableBalance, (500 * USD) as i64);
}

// ── 2. Long, no orders ─────────────────────────────────────────────────────

#[test]
fn long_without_orders_has_im_equal_pim_and_no_open_order_margin() {
    let mut ctx = make_ctx();
    add_market(
        &mut ctx,
        MARKET_A,
        A_BASE_DECIMALS,
        A_PRICE_DECIMALS,
        11_000,
    ); // mark $110
       // Long 2 opened at $100 with $200 of margin at leverage 1.
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        2,
        -200 * USD as i64,
        200 * USD as i64,
        1,
    );

    let i = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(i.notional, 220 * USD, "trunc(|2| * $110)");
    assert_eq!(i.unrealizedProfit, 20 * USD as i64, "2 * ($110 - $100)");
    assert_eq!(
        i.isolatedMargin,
        i.positionMargin + i.unrealizedProfit,
        "isolatedMargin = isolatedWallet + uPnL"
    );
    assert_eq!(i.isolatedMargin, 220 * USD as i64);
    assert_eq!(i.positionInitialMargin, 220 * USD);
    assert_eq!(
        i.initialMargin, i.positionInitialMargin,
        "no orders => IM == PIM"
    );
    assert_eq!(i.openOrderInitialMargin, 0);
    // Default tier table is a single tier {0, maxLeverage 3} => mmr = 1/(2*3): 220e6 / 6.
    assert_eq!(i.maintMargin, 36_666_666);
}

// ── 3. The joint max() really switches branches ────────────────────────────

#[test]
fn joint_max_switches_branches_between_bid_and_ask() {
    // Long 2 at mark $100 => N = $200. One resting sell of 5 @ $100 => Ask = $500.
    //   ask branch |N - Ask| = |200 - 500| = 300
    //   bid branch |N + Bid| =  200 + Bid
    // So the ask branch leads until Bid > $100. Mirrors the reference doc's run2 P2→P3→P4:
    // the winning branch flips by changing ONLY the buy quantity.
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        2,
        -200 * USD as i64,
        200 * USD as i64,
        1,
    );
    let sells = [entry(1, P100, 5)];

    // (a) ask only — the ask branch wins outright.
    set_orders(&mut ctx, ALICE, MARKET_A, &[], &sells);
    let ask_only = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!((ask_only.bidNotional, ask_only.askNotional), (0, 500 * USD));
    assert_eq!(ask_only.initialMargin, 300 * USD);
    assert_eq!(ask_only.positionInitialMargin, 200 * USD);
    assert_eq!(ask_only.openOrderInitialMargin, 100 * USD);

    // (b) add a REAL $60 buy — the ask branch still wins, so the buy contributes EXACTLY zero.
    // This is the doc's sharpest step (run2 P3): it refutes "the two sides add independently",
    // which would have charged for the buy.
    set_orders(&mut ctx, ALICE, MARKET_A, &[entry(2, P60, 1)], &sells);
    let ask_wins = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(
        (ask_wins.bidNotional, ask_wins.askNotional),
        (60 * USD, 500 * USD)
    );
    assert_eq!(
        ask_wins.openOrderInitialMargin, ask_only.openOrderInitialMargin,
        "a live $60 buy under a winning ask branch must cost exactly 0"
    );
    assert_eq!(ask_wins.initialMargin, 300 * USD);

    // (c) grow only the buy side to $400 — now |N + Bid| = 600 > 300 and the branch FLIPS.
    set_orders(&mut ctx, ALICE, MARKET_A, &[entry(2, P100, 4)], &sells);
    let bid_wins = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(
        (bid_wins.bidNotional, bid_wins.askNotional),
        (400 * USD, 500 * USD)
    );
    assert_eq!(bid_wins.initialMargin, 600 * USD);
    assert_eq!(bid_wins.positionInitialMargin, 200 * USD, "PIM never moved");
    assert_eq!(bid_wins.openOrderInitialMargin, 400 * USD);
    assert_ne!(
        bid_wins.openOrderInitialMargin, ask_wins.openOrderInitialMargin,
        "the max() must have switched branches"
    );
}

#[test]
fn joint_max_uses_the_signed_notional_so_a_short_charges_on_the_ask_side() {
    // The reference doc's samples are all LONGS and it lists short-side signs as unverified, so
    // this pins OUR decision: `N` enters the joint max SIGNED. For a short 2 at mark $100
    // (N = -$200) a resting sell of 3 @ $100 grows the short exposure to |-200 - 300| = 500,
    // while the buy branch |-200 + 0| = 200 is below |N|. An UNSIGNED N would have reported
    // max(|200|, |200 - 300|) = 200 == PIM, i.e. ooIM 0 — understating the very exposure the
    // ask branch exists to measure.
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        -2,
        200 * USD as i64,
        200 * USD as i64,
        1,
    );
    set_orders(&mut ctx, ALICE, MARKET_A, &[], &[entry(1, P100, 3)]);

    let i = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(i.notional, 200 * USD, "notional itself is unsigned");
    assert_eq!(i.positionInitialMargin, 200 * USD);
    assert_eq!(i.initialMargin, 500 * USD);
    assert_eq!(i.openOrderInitialMargin, 300 * USD);
}

/// ⚠️ **CHARACTERISATION OF AN EXTRAPOLATION — not a Binance measurement.**
///
/// The full pipeline (`positionAmt` → `N` → the joint `max()` → `ooIM`) on a SHORT position, for
/// each shape already pinned on the long side. All ten mainnet runs behind this formula used a
/// LONG, so *none* of the numbers below has ever been observed on Binance;
/// `misc/binance-margin-verified-model.md` §6 (「空头侧符号」) lists the short-side form of the joint
/// `max()` as extrapolated and docs commit `8d179c0` UPGRADED it to a **BLOCKING** open item (a
/// ~0.064 USDT three-arm probe to settle it is designed in `misc/binance-flip-and-admission.md`
/// §3.3). A failure here means "our short side moved", NOT "we diverged from Binance".
///
/// The four shapes, on a SHORT 2 at mark $100 (`N = −$200`, `L = 1` ⇒ `PIM = $200`) with
/// `lastTraded = $100` ⇒ `T = ROUND_UP($100 × 1.0015) = $100.15`:
///
/// ```text
/// resting BUYS         Bid        bid branch          IM      ooIM   shape
/// 2 @ $100           $200    |−200 + 200| =   0     $200         0   buys would CLOSE it
/// 4 @ $100           $400    |−200 + 400| = 200     $200         0   last free unit (2|N|)
/// 4 @ $100 + 1 @ $.01  +$0.01 |…| = 200.01       $200.01    $0.01   charging begins
/// 6 @ $100           $600    |−200 + 600| = 400     $400      $200   buys FLIP it: residual only
/// ```
#[test]
fn the_short_side_of_the_joint_max_is_an_extrapolation_pinned_shape_by_shape() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    // `T` must be strictly above the $100 order price so the buy side's "no markup" is a real
    // claim and not a degenerate coincidence: at `lastTraded == 0`, `T` collapses to the mark.
    storage::save_last_traded_price(&mut ctx, MARKET_A, P100).unwrap();
    // SHORT 2 opened at $100 with $200 of margin at leverage 1.
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        -2,
        200 * USD as i64,
        200 * USD as i64,
        1,
    );

    // ── (a) buys that would CLOSE the short: free, and priced at their OWN limit ──
    set_orders(&mut ctx, ALICE, MARKET_A, &[entry(1, P100, 2)], &[]);
    let close = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(close.positionAmt, -2);
    assert_eq!(close.notional, 200 * USD, "notional itself is unsigned");
    assert_eq!(close.positionInitialMargin, 200 * USD);
    assert_eq!(
        close.bidNotional,
        200 * USD,
        "the BUY side carries no markup even resting BELOW T — §3.8"
    );
    assert_eq!(close.initialMargin, 200 * USD, "|−200 + 200| = 0 < |N|");
    assert_eq!(close.openOrderInitialMargin, 0);

    // ── (b) the far endpoint of the free band: Bid == 2|N| exactly (a TIE, still free) ──
    set_orders(&mut ctx, ALICE, MARKET_A, &[entry(1, P100, 4)], &[]);
    let band_end = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(band_end.bidNotional, 400 * USD);
    assert_eq!(
        band_end.initialMargin,
        200 * USD,
        "|−200 + 400| == |N|: the branches tie and ooIM is still 0"
    );
    assert_eq!(band_end.openOrderInitialMargin, 0);

    // ── (c) one tick past the band: charging begins, at the smallest representable step ──
    set_orders(
        &mut ctx,
        ALICE,
        MARKET_A,
        &[entry(1, P100, 4), entry(2, 1, 1)], // + 1 unit @ $0.01, price-DESC
        &[],
    );
    let past_band = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(past_band.bidNotional, 400 * USD + 10_000);
    assert_eq!(past_band.initialMargin, 200 * USD + 10_000);
    assert_eq!(
        past_band.openOrderInitialMargin, 10_000,
        "$0.01 past 2|N| costs exactly $0.01 — the band has a hard edge"
    );

    // ── (d) buys big enough to FLIP the short: only the residual LONG exposure is charged ──
    set_orders(&mut ctx, ALICE, MARKET_A, &[entry(1, P100, 6)], &[]);
    let flip = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(flip.bidNotional, 600 * USD);
    assert_eq!(flip.initialMargin, 400 * USD, "|−200 + 600| = 400");
    assert_eq!(
        flip.openOrderInitialMargin,
        200 * USD,
        "the residual after the flip, NOT the whole $600 buy leg (which would be $600 of ooIM)"
    );
    assert_ne!(flip.openOrderInitialMargin, 600 * USD);
}

/// The mirror BREAKS at the engine level, by exactly the Assuming-Price uplift — and that break is
/// the one part of the short-side story that IS measured.
///
/// [`crate::math::open_order_margin`] itself is exactly sign-symmetric: `ooIM(N, Bid, Ask)` equals
/// `ooIM(−N, Ask, Bid)`. But the uplift is keyed to the ORDER'S side, not the position's — a buy's
/// Assuming Price is its own limit, a sell's is `max(T, limit)`
/// (`misc/binance-flip-and-admission.md` §3.8: 「买单侧完全没有加成」). So mirroring a whole
/// `(position, book)` does NOT preserve `ooIM`, and hedging a short with buys is strictly cheaper
/// than hedging a long with sells.
///
/// Pinned on the flip shape at `lastTraded = $100` ⇒ `T = $100.15`:
///
/// ```text
/// SHORT 2 + 6 buys  @ $100 → Bid = 6 × $100    = $600.00 → ooIM = $200.00
/// LONG  2 + 6 sells @ $100 → Ask = 6 × $100.15 = $600.90 → ooIM = $200.90
///                                                 difference = 6 × $0.15 = $0.90, exactly
/// ```
///
/// This asymmetry is deliberate and measured. It must NOT be "made consistent" by adding a markup
/// to the buy side (§3.8 measures there is none) or by dropping it from the sell side (R9/R10
/// measure that there is).
#[test]
fn mirroring_a_short_onto_a_long_costs_exactly_the_sell_side_uplift_more() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    storage::save_last_traded_price(&mut ctx, MARKET_A, P100).unwrap();

    // SHORT 2 with 6 buys @ $100 — the flip shape from the test above.
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        -2,
        200 * USD as i64,
        200 * USD as i64,
        1,
    );
    set_orders(&mut ctx, ALICE, MARKET_A, &[entry(1, P100, 6)], &[]);
    let short = margin_info(&mut ctx, ALICE, MARKET_A);

    // The mirror image: LONG 2 with 6 sells @ $100.
    set_position(
        &mut ctx,
        BOB,
        MARKET_A,
        2,
        -(200 * USD as i64),
        200 * USD as i64,
        1,
    );
    set_orders(&mut ctx, BOB, MARKET_A, &[], &[entry(1, P100, 6)]);
    let long = margin_info(&mut ctx, BOB, MARKET_A);

    // Same |N|, same PIM, same order notional at the LIMIT price.
    assert_eq!(short.notional, long.notional);
    assert_eq!(short.positionInitialMargin, long.positionInitialMargin);
    assert_eq!(short.bidNotional, 600 * USD, "buys: no markup");
    assert_eq!(long.askNotional, 600 * USD + 900_000, "sells: 6 × $0.15");

    // ...and the ooIM differs by exactly the uplift, nothing else.
    assert_eq!(short.openOrderInitialMargin, 200 * USD);
    assert_eq!(long.openOrderInitialMargin, 200 * USD + 900_000);
    assert_eq!(
        long.openOrderInitialMargin - short.openOrderInitialMargin,
        900_000,
        "the mirror breaks by exactly the sell-side Assuming-Price uplift and by nothing else"
    );
}

// ── 4. ROUND_UP is really round-up ─────────────────────────────────────────

#[test]
fn initial_margins_round_up_and_open_order_margin_is_a_difference_of_two_round_ups() {
    // Long 1 at mark $100, leverage 3 => N/L = 100_000_000 / 3 = 33_333_333.33…
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        1,
        -100 * USD as i64,
        100 * USD as i64 / 3,
        3,
    );

    let bare = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(bare.notional, 100 * USD);
    assert_eq!(
        bare.positionInitialMargin, 33_333_334,
        "ROUND_UP(100e6/3); floor/truncate would give 33_333_333"
    );
    assert_ne!(bare.positionInitialMargin, 33_333_333);
    assert_eq!(bare.initialMargin, 33_333_334);
    assert_eq!(bare.openOrderInitialMargin, 0);

    // Add a $100 buy: Bid = 100e6, so max(|N+Bid|, |N-Ask|) = 200e6.
    //   IM   = ROUND_UP(200e6 / 3) = 66_666_667
    //   PIM  = ROUND_UP(100e6 / 3) = 33_333_334
    //   ooIM = IM - PIM            = 33_333_333
    // The convenience form the doc warns about, ROUND_UP(max(0, Bid, Ask - 2N) / L) =
    // ROUND_UP(100e6 / 3), would give 33_333_334 — off by exactly 1 ulp, because
    // ceil(a) - ceil(b) != ceil(a - b). This assertion is what pins `IM - PIM` as the rule.
    set_orders(&mut ctx, ALICE, MARKET_A, &[entry(1, P100, 1)], &[]);
    let i = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(i.bidNotional, 100 * USD);
    assert_eq!(i.initialMargin, 66_666_667);
    assert_eq!(i.positionInitialMargin, 33_333_334);
    assert_eq!(
        i.openOrderInitialMargin, 33_333_333,
        "IM - PIM; the one-round-up convenience form would give 33_333_334"
    );
    assert_ne!(i.openOrderInitialMargin, 33_333_334);
}

// ── 5. uPnL truncates toward zero, not floor ───────────────────────────────

#[test]
fn unrealized_profit_truncates_toward_zero_not_floor() {
    // Only a NEGATIVE signed notional can separate the two, so this needs a SHORT on a
    // fractional grid. Market B: calc_value(price, qty) = price * qty / 100.
    //   |amount| * mark = 3 * 12_345 = 37_035  =>  370.35 quote units
    //   truncate-toward-zero: signed notional = -370      (what Binance does)
    //   floor:                signed notional = -371      (refuted, doc §4, 11/11 samples:
    //                                                      -0.08323233884 -> -0.08323233)
    // With v_quote = +300 the two models differ in the REPORTED PnL: -70 vs -71.
    let mut ctx = make_ctx();
    add_market(
        &mut ctx,
        MARKET_B,
        B_BASE_DECIMALS,
        B_PRICE_DECIMALS,
        12_345,
    );
    set_position(&mut ctx, ALICE, MARKET_B, -3, 300, 200, 1);

    let i = margin_info(&mut ctx, ALICE, MARKET_B);
    assert_eq!(i.notional, 370, "trunc(3 * 12_345 / 100) = trunc(370.35)");
    assert_eq!(
        i.unrealizedProfit, -70,
        "truncate toward zero: -370 + 300; floor would give -371 + 300 = -71"
    );
    assert_ne!(i.unrealizedProfit, -71);
    // Self-consistency of the sign convention: the SAME truncated notional, negated.
    assert_eq!(i.unrealizedProfit, -(i.notional as i64) + i.vQuoteBalance);
    assert_eq!(i.isolatedMargin, 200 - 70);
}

// ── 6. availableBalance is signed and is NOT clamped at zero ───────────────

#[test]
fn available_balance_is_reported_negative_not_clamped() {
    // Binance clamps this field at zero (measured: reports 0.00000000 where the true value is
    // -0.00085981), which its own reference doc calls out as making the field unusable for
    // deciding whether an account is under-covered. Ours is int64 and stays signed.
    //
    // Two properties, and they are separate:
    //
    //   (a) `availableBalance` subtracts the open-order requirement EXACTLY ONCE. The wallet is
    //       the CROSS wallet — net of the position allocation, NOT of the resting orders — so the
    //       single arithmetic subtraction here is the whole charge.
    //   (b) when the wallet genuinely IS negative, the field passes the sign through.
    //
    // (a) — driven through the REAL engine: ALICE funds $500 and rests a $400 buy. Nothing is
    // debited (the wallet stays $500); the $400 shows up as a REQUIREMENT, leaving $100 available.
    //
    // CHANGED BY THE ESCROW REMOVAL: the wallet field (now `totalCrossWalletBalance`) was 100
    // here (the escrow had physically
    // removed $400) and `availableBalance` was the wallet itself. It is now 500 and 100. Same
    // spendable headroom, reached the Binance way — which is the point of the migration.
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    fund(&mut ctx, ALICE, 500 * USD);
    place(&mut ctx, ALICE, 0, P100, 4);

    let i = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(i.bidNotional, 400 * USD);
    assert_eq!(i.openOrderInitialMargin, 400 * USD);

    let a = account_margin(&mut ctx, ALICE, &[MARKET_A]);
    assert_eq!(
        a.totalCrossWalletBalance,
        500 * USD as i64,
        "resting an order debits nothing"
    );
    assert_eq!(a.totalOpenOrderInitialMargin, 400 * USD);
    assert_eq!(
        a.availableBalance,
        100 * USD as i64,
        "spendable headroom = wallet - ooIM, charged exactly once"
    );

    // (b) a genuinely negative wallet — reachable: a close-path fee can drive it there — is
    // reported negative, not clamped. Binance measured 0.00000000 where the true value was
    // -0.00085981, which its own doc calls out as making the field useless for detecting
    // under-coverage.
    let mut ctx2 = make_ctx();
    setup_a(&mut ctx2);
    storage::save_account(
        &mut ctx2,
        ALICE,
        UserAccount {
            perp_wallet_balance: -(3 * USD as i64),
            ..UserAccount::default()
        },
    )
    .unwrap();
    let a2 = account_margin(&mut ctx2, ALICE, &[MARKET_A]);
    assert_eq!(a2.availableBalance, -(3 * USD as i64));
    assert!(a2.availableBalance < 0, "must not be clamped at zero");
}

/// Every balance-like field on `getAccountMargin` is `int64` and passes a negative through. There is
/// no CLAMPED surface left: `getAccount` used to clamp (as `availablePerpBalance`) and
/// `AccountBalanceChanged` used to clamp (as `uint64 perpWalletBalance`); neither does now.
/// `visible_perp_wallet_balance` survives only as the contrast asserted below — what a clamped
/// reading WOULD have said.
#[test]
fn cross_wallet_and_margin_balance_are_signed_where_a_clamped_reading_would_not_be() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    storage::save_account(
        &mut ctx,
        ALICE,
        UserAccount {
            perp_wallet_balance: -(7 * USD as i64),
            ..UserAccount::default()
        },
    )
    .unwrap();

    let a = account_margin(&mut ctx, ALICE, &[MARKET_A]);
    assert_eq!(a.totalCrossWalletBalance, -(7 * USD as i64));
    assert_eq!(a.crossMarginBalance, -(7 * USD as i64));
    assert_eq!(a.availableBalance, -(7 * USD as i64));
    assert_eq!(
        storage::load_account_ref(&mut ctx, ALICE)
            .unwrap()
            .visible_perp_wallet_balance(),
        0,
        "what a clamped reading would report, for contrast — no published surface does this"
    );
}

// ── 7. THE FLIP THE MIGRATION WAS FOR ──────────────────────────────────────

#[test]
fn a_resting_order_that_can_flip_the_position_is_charged_the_joint_max() {
    // ─────────────────────────────────────────────────────────────────────────────────────
    // The book shape that motivated the whole derived-ooIM migration, kept as a behaviour pin.
    //
    // Setup (the reference doc's own worked example, `binance-margin-verified-model.md` §5):
    // a LONG 2 at $100 with a resting SELL of 5 @ $100, leverage 1, entry = mark = limit = $100.
    // The sell can flip the position's sign, which is exactly where the retired escrow and
    // Binance's formula parted company.
    //
    //   Binance (joint requirement over position AND orders, netting the flip) — what we now do.
    //   The sell rests AT the last traded price, so its Assuming Price is
    //   `T = ROUND_UP($100 × 1.0015) = $100.15` (the tick grid is cents, so `10_015` exactly),
    //   NOT its own limit:
    //     N   = $200,  Bid = $0,  Ask = 5 × $100.15 = $500.75
    //     IM  = ROUND_UP(max(|200 + 0|, |200 - 500.75|) / 1) = $300.75
    //     PIM = ROUND_UP(200 / 1)                            = $200
    //     ooIM = IM - PIM                                    = $100.75
    //     TOTAL capital tied up = positionMargin $200 + ooIM $100.75 = IM = $300.75
    //
    //   The retired escrow charged `c_notional = max(S + B', B + S') = $300` for the ORDERS
    //   ALONE, on top of the $200 position margin — $500 of capital, 1.67x. The $200 gap was
    //   exactly `positionMargin`, and it was structural, not rounding: when the sell fills the
    //   long closes and its $200 of margin is released, which Binance's single joint `max()`
    //   nets by construction and a per-side reservation bucket cannot.
    //
    //   MEASURED HERE: the account now ties up $300.75 total on this book instead of $500. The
    //   loosening is deliberate and was accepted with the migration; the $0.75 is the
    //   Assuming-Price markup, which pulls back a sliver of it on any sell resting at or below
    //   `max(lastTraded × 1.0015, mark)`.
    // ─────────────────────────────────────────────────────────────────────────────────────
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    fund(&mut ctx, ALICE, 10_000 * USD);
    fund(&mut ctx, BOB, 10_000 * USD);

    // BOB rests the ask; ALICE takes it and is long 2 at $100 with $200 of margin.
    place(&mut ctx, BOB, 1, P100, 2);
    place(&mut ctx, ALICE, 0, P100, 2);
    // ALICE now rests a sell of 5 @ $100 — the flip.
    place(&mut ctx, ALICE, 1, P100, 5);

    let i = margin_info(&mut ctx, ALICE, MARKET_A);

    // Inputs, so the numbers above are checkable.
    assert_eq!(i.markPrice, P100);
    assert_eq!(i.positionAmt, 2);
    assert_eq!(i.vQuoteBalance, -(200 * USD as i64));
    assert_eq!(i.leverage, 1);
    assert_eq!(i.bidNotional, 0);
    assert_eq!(
        i.askNotional, 500_750_000,
        "the ASSUMING-price aggregate: 5 × ROUND_UP($100 × 1.0015), not 5 × the $100 limit"
    );
    assert_eq!(i.notional, 200 * USD);
    assert_eq!(i.unrealizedProfit, 0, "mark == entry");

    // The Binance numbers.
    assert_eq!(i.positionInitialMargin, 200 * USD);
    assert_eq!(i.initialMargin, 300_750_000);
    assert_eq!(i.openOrderInitialMargin, 100_750_000);

    // Total capital tied up == IM, exactly. This identity is the migration: `positionMargin` is
    // physically held, `ooIM` is arithmetically withheld, and together they are the joint
    // requirement — no third bucket, no double count.
    assert_eq!(i.positionMargin, 200 * USD as i64);
    let a = account_margin(&mut ctx, ALICE, &[MARKET_A]);
    assert_eq!(
        i.positionMargin + a.totalOpenOrderInitialMargin as i64,
        a.totalInitialMargin as i64
    );
    assert_eq!(a.totalInitialMargin, 300_750_000);

    // And in the wallet: $10 000 - $200 (the position, physically debited at open). The $100.75
    // ooIM is NOT debited — it is subtracted on read.
    assert_eq!(a.totalCrossWalletBalance, 9_800 * USD as i64);
    assert_eq!(a.availableBalance, 9_800 * USD as i64 - 100_750_000);
    // The escrow basis left only $9 500 spendable on this same book (it debited $200 + $300).
    assert_eq!(
        a.availableBalance - 9_500 * USD as i64,
        200 * USD as i64 - 750_000,
        "the flip's released position margin, no longer charged twice, less the $0.75 markup"
    );
}

// ── 8. Self-consistency: the outputs are recomputable from the inputs ──────

/// Independently re-derive every derived field from the six reported inputs, using explicit
/// i128 arithmetic and explicit rounding — deliberately NOT calling the engine's helpers, so
/// this is a real cross-check rather than a tautology.
fn recompute(
    i: &getMarginInfoReturn,
    base_decimals: u32,
    price_decimals: u32,
) -> (u64, i64, i64, u64, u64, u64) {
    // Exact for either fixture: one numerator, one denominator, no pre-scaling.
    let num = (i.positionAmt.unsigned_abs() as i128) * (i.markPrice as i128) * 1_000_000;
    let den = 10i128.pow(price_decimals) * 10i128.pow(base_decimals);
    let notional = (num / den) as u64; // integer division on non-negatives == truncate
    let signed = if i.positionAmt < 0 {
        -(notional as i128)
    } else {
        notional as i128
    };
    let upnl = (signed + i.vQuoteBalance as i128) as i64;
    let lev = i.leverage.max(1) as u128;
    let pim = ((notional as u128).div_ceil(lev)) as u64;
    let bid_branch = (signed + i.bidNotional as i128).unsigned_abs();
    let ask_branch = (signed - i.askNotional as i128).unsigned_abs();
    let im = (bid_branch.max(ask_branch).div_ceil(lev)) as u64;
    let oo_im = im - pim;
    // Single default tier {0, maxLeverage 3} => mmr = 1/(2*3).
    let maint = notional / 6;
    (notional, upnl, im as i64, pim, oo_im, maint)
}

#[test]
fn outputs_are_recomputable_from_the_reported_inputs() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    add_market(
        &mut ctx,
        MARKET_B,
        B_BASE_DECIMALS,
        B_PRICE_DECIMALS,
        12_345,
    );
    fund(&mut ctx, ALICE, 5_000 * USD);

    // Market A: long 3 at leverage 3, both sides of the book resting.
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        3,
        -(290 * USD as i64),
        97 * USD as i64,
        3,
    );
    set_orders(
        &mut ctx,
        ALICE,
        MARKET_A,
        &[entry(1, P100, 2), entry(2, P60, 1)],
        &[entry(3, P100, 4), entry(4, 12_000, 3)],
    );
    // Market B: short 3 on the fractional grid.
    set_position(&mut ctx, ALICE, MARKET_B, -3, 300, 200, 1);

    let a_info = margin_info(&mut ctx, ALICE, MARKET_A);
    let b_info = margin_info(&mut ctx, ALICE, MARKET_B);

    for (info, bd, pd) in [
        (&a_info, A_BASE_DECIMALS, A_PRICE_DECIMALS),
        (&b_info, B_BASE_DECIMALS, B_PRICE_DECIMALS),
    ] {
        let (notional, upnl, im, pim, oo_im, maint) = recompute(info, bd, pd);
        assert_eq!(info.notional, notional);
        assert_eq!(info.unrealizedProfit, upnl);
        assert_eq!(info.isolatedMargin, info.positionMargin + upnl);
        assert_eq!(info.initialMargin as i64, im);
        assert_eq!(info.positionInitialMargin, pim);
        assert_eq!(info.openOrderInitialMargin, oo_im);
        assert_eq!(info.maintMargin, maint);
        assert_eq!(
            info.initialMargin,
            info.positionInitialMargin + info.openOrderInitialMargin,
            "IM == PIM + ooIM, the identity Binance's account level relies on"
        );
    }

    // Account totals are exactly the per-market sums, in the order given.
    let acc = account_margin(&mut ctx, ALICE, &[MARKET_A, MARKET_B]);
    assert_eq!(
        acc.totalInitialMargin,
        a_info.initialMargin + b_info.initialMargin
    );
    assert_eq!(
        acc.totalPositionInitialMargin,
        a_info.positionInitialMargin + b_info.positionInitialMargin
    );
    assert_eq!(
        acc.totalOpenOrderInitialMargin,
        a_info.openOrderInitialMargin + b_info.openOrderInitialMargin
    );
    assert_eq!(
        acc.totalInitialMargin,
        acc.totalPositionInitialMargin + acc.totalOpenOrderInitialMargin
    );
    assert_eq!(
        acc.totalMaintMargin,
        a_info.maintMargin + b_info.maintMargin
    );
    assert_eq!(
        acc.totalUnrealizedProfit,
        a_info.unrealizedProfit + b_info.unrealizedProfit
    );
    assert_eq!(
        acc.crossMarginBalance,
        acc.totalCrossWalletBalance + acc.totalUnrealizedProfit
    );
    // `availableBalance = walletBalance - Σ ooIM`, Binance's identity literally. The wallet is
    // net of the POSITION allocation only; the open-order requirement is subtracted here and
    // nowhere else.
    assert_eq!(
        acc.availableBalance,
        acc.totalCrossWalletBalance - acc.totalOpenOrderInitialMargin as i64
    );
    assert!(
        acc.totalOpenOrderInitialMargin > 0,
        "the identity above is only meaningful with orders resting"
    );

    // Order of the id list does not change any total.
    let reversed = account_margin(&mut ctx, ALICE, &[MARKET_B, MARKET_A]);
    assert_eq!(as_tuple(&reversed), as_tuple(&acc));
}

// ── 9. `getAccount`: the SAME walkers, driven by the per-user market index ─
//
// `getAccount` and `getAccountMargin` share one set of Σ walkers
// (`margin_view::account_margin_scalars`) and differ ONLY in where the market set comes from. These
// tests exercise that seam: the index path's edges (empty, at the 16-market cap), the fields the
// index makes possible (`totalWalletBalance` needs `Σ isolatedWallet` over the WHOLE set), the
// clamp that was dropped, and the round-trip against per-market `getMarginInfo`.

/// An account with NO markets: the index is empty, every total is 0, and the call SUCCEEDS.
///
/// The empty index is the common case (a funded account that has not traded), so it must not be an
/// error path. It also has to be distinguishable from a market set that happened to sum to zero —
/// `marketIds` empty is that signal.
#[test]
fn get_account_on_an_empty_index_is_all_zero_and_does_not_revert() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    fund(&mut ctx, ALICE, 500 * USD);

    let a = get_account(&mut ctx, ALICE);
    assert!(a.marketIds.is_empty(), "no positions, no resting orders");
    // The wallet is the only non-zero thing, and gross == cross with no silos.
    assert_eq!(a.totalCrossWalletBalance, (500 * USD) as i64);
    assert_eq!(a.totalWalletBalance, (500 * USD) as i64);
    assert_eq!(a.totalMarginBalance, (500 * USD) as i64);
    assert_eq!(a.availableBalance, (500 * USD) as i64);
    assert_eq!(a.totalUnrealizedProfit, 0);
    assert_eq!(a.totalInitialMargin, 0);
    assert_eq!(a.totalPositionInitialMargin, 0);
    assert_eq!(a.totalOpenOrderInitialMargin, 0);
    assert_eq!(a.totalMaintMargin, 0);

    // A never-seen user is the same shape, and still not an error.
    let never = get_account(&mut ctx, BOB);
    assert!(never.marketIds.is_empty());
    assert_eq!(never.usdcBalance, U256::ZERO);
    assert_eq!(never.totalWalletBalance, 0);
    assert_eq!(never.totalMarginBalance, 0);
    assert_eq!(never.availableBalance, 0);
}

/// An account at the `MAX_USER_MARKETS` (16) cap. The walk is bounded by the index, so this is the
/// worst case the flat gas has to cover — and every total must still be the full 16-market sum,
/// with `marketIds` reporting all 16 ascending.
#[test]
fn get_account_walks_the_index_at_the_sixteen_market_cap() {
    let mut ctx = make_ctx();
    fund(&mut ctx, ALICE, 100_000 * USD);
    let ids: Vec<u64> = (1..=MAX_USER_MARKETS as u64).collect();
    for &m in &ids {
        add_market(&mut ctx, m, A_BASE_DECIMALS, A_PRICE_DECIMALS, P100);
        // Long 1 @ mark $100, fully margined at leverage 1, plus one resting buy so every market
        // contributes a non-zero ooIM term as well as a silo.
        set_position(
            &mut ctx,
            ALICE,
            m,
            1,
            -(100 * USD as i64),
            100 * USD as i64,
            1,
        );
        set_orders(&mut ctx, ALICE, m, &[entry(1, P60, 1)], &[]);
    }
    assert_eq!(
        storage::load_user_markets(&mut ctx, ALICE).unwrap(),
        ids,
        "the index is full and ascending"
    );

    let a = get_account(&mut ctx, ALICE);
    assert_eq!(a.marketIds, ids, "all 16 markets, in index order");
    let n = MAX_USER_MARKETS as i64;
    // Σ over 16 identical markets: silo $100 each, mark == entry so no uPnL.
    assert_eq!(a.totalUnrealizedProfit, 0);
    assert_eq!(a.totalCrossWalletBalance, (100_000 * USD) as i64);
    assert_eq!(
        a.totalWalletBalance,
        (100_000 * USD) as i64 + n * 100 * USD as i64
    );
    assert_eq!(a.totalMarginBalance, a.totalWalletBalance);
    assert_eq!(
        a.totalPositionInitialMargin,
        MAX_USER_MARKETS as u64 * 100 * USD
    );
    assert!(
        a.totalOpenOrderInitialMargin > 0,
        "each market has a resting buy, so the ooIM total is a real 16-term sum"
    );
    assert_eq!(
        a.totalInitialMargin,
        a.totalPositionInitialMargin + a.totalOpenOrderInitialMargin
    );
    assert_eq!(
        a.availableBalance,
        a.totalCrossWalletBalance - a.totalOpenOrderInitialMargin as i64
    );
}

/// A NEGATIVE cross wallet reports NEGATIVE — the whole reason the clamp was dropped — and it
/// propagates into `totalWalletBalance`, `totalMarginBalance` and `availableBalance` rather than
/// being absorbed at 0 in one of them.
///
/// The old `uint64 availablePerpBalance` floored here, which made an under-covered account
/// indistinguishable from an exactly-covered one through this selector. That is the identical trap
/// `misc/binance-v3-account-balance-field-reference.md` §1 identifies in Binance's own clamped
/// `availableBalance` (reported `0.00000000`, true value `−0.00085981`): read it for headroom, but
/// you cannot read it for coverage. We do not inherit it.
#[test]
fn get_account_reports_a_negative_cross_wallet_unclamped() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    // A $7 deficit and a silo of $100 backing a long 1 @ $100 (mark == entry, so no uPnL).
    storage::save_account(
        &mut ctx,
        ALICE,
        UserAccount {
            perp_wallet_balance: -(7 * USD as i64),
            ..UserAccount::default()
        },
    )
    .unwrap();
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        1,
        -(100 * USD as i64),
        100 * USD as i64,
        1,
    );

    let a = get_account(&mut ctx, ALICE);
    assert_eq!(
        a.totalCrossWalletBalance,
        -(7 * USD as i64),
        "the sign survives"
    );
    assert_eq!(a.availableBalance, -(7 * USD as i64), "no orders ⇒ ooIM 0");
    // The silo is real money and is ADDED to the negative cross wallet: gross = −$7 + $100 = $93.
    assert_eq!(a.totalWalletBalance, 93 * USD as i64);
    assert_eq!(a.totalMarginBalance, 93 * USD as i64);
    // And the deficit is NOT hidden inside the gross figure — the two differ by exactly the silo.
    assert_eq!(
        a.totalWalletBalance - a.totalCrossWalletBalance,
        100 * USD as i64,
        "totalWalletBalance − totalCrossWalletBalance == Σ isolatedWallet, Binance's identity"
    );
}

/// A SHORT position whose silo is UNDER-FUNDED (model M1: a fill funds `pos.margin` with
/// `min(requirement, cash at hand)` and leaves the silo short). Both readings of "short silo" hold
/// in this one fixture — the position is short AND its margin sits below its
/// `positionInitialMargin` — and `totalWalletBalance` / `totalMarginBalance` must reflect the
/// ACTUAL silo, not the requirement.
///
/// Reporting the requirement instead would overstate the account's wallet by the shortfall, i.e.
/// mint money in the view. `positionMargin < positionInitialMargin` is a legitimate post-fill state
/// and must not be read as an error.
#[test]
fn get_account_reflects_a_short_under_funded_silo_at_its_actual_value() {
    let mut ctx = make_ctx();
    // Mark $60, so a short 2 opened at $100 carries +$80 of unrealized gain.
    add_market(&mut ctx, MARKET_A, A_BASE_DECIMALS, A_PRICE_DECIMALS, P60);
    fund(&mut ctx, ALICE, 500 * USD);
    // Short 2 @ $100 (v_quote = +$200) with only $100 in the silo, against a
    // `positionInitialMargin` of $120 at mark $60 and leverage 1 — genuinely SHORT by $20.
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        -2,
        200 * USD as i64,
        100 * USD as i64,
        1,
    );

    let i = margin_info(&mut ctx, ALICE, MARKET_A);
    assert_eq!(i.positionAmt, -2, "SHORT");
    assert_eq!(i.notional, 120 * USD, "trunc(|−2| × $60)");
    assert_eq!(i.unrealizedProfit, 80 * USD as i64, "−$120 + $200");
    assert_eq!(i.positionInitialMargin, 120 * USD, "ROUND_UP($120 / 1)");
    assert_eq!(i.positionMargin, 100 * USD as i64);
    assert!(
        i.positionMargin < i.positionInitialMargin as i64,
        "the silo is UNDER-FUNDED: reported as the ACTUAL allocation, never as the requirement"
    );

    let a = get_account(&mut ctx, ALICE);
    assert_eq!(a.marketIds, vec![MARKET_A]);
    assert_eq!(a.totalCrossWalletBalance, 500 * USD as i64);
    // Gross wallet carries the short's ACTUAL silo ($100), not its requirement ($120) — using the
    // requirement would mint $20 in the view.
    assert_eq!(
        a.totalWalletBalance,
        600 * USD as i64,
        "$500 cross + $100 silo"
    );
    assert_eq!(a.totalUnrealizedProfit, 80 * USD as i64);
    // Equity is GROSS-based: $600 + $80. Note it is NOT cross + uPnL ($580) — that difference is
    // exactly why `getAccountMargin`'s field is named `crossMarginBalance`.
    assert_eq!(a.totalMarginBalance, 680 * USD as i64);
    assert_eq!(
        a.totalMarginBalance,
        a.totalCrossWalletBalance + i.isolatedMargin,
        "equity == cross wallet + Σ isolatedMargin (silo + uPnL), the per-market cross-check"
    );
}

/// THE ROUND-TRIP: every account-level total recomputed independently from per-market
/// `getMarginInfo` calls over `marketIds`, and asserted equal.
///
/// This is our answer to reference-doc §7.1 item 3 — Binance's v3 `positions[]` reports derived
/// quantities while deleting every input, so a caller cannot self-check. Here `marketIds` names the
/// exact market set that was summed, `getMarginInfo` returns the raw inputs per market, and the four
/// balance identities are stated below in the only place they are implemented. A caller can do
/// literally this.
#[test]
fn get_account_totals_equal_the_sum_of_per_market_get_margin_info() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    add_market(
        &mut ctx,
        MARKET_B,
        B_BASE_DECIMALS,
        B_PRICE_DECIMALS,
        12_345,
    );
    fund(&mut ctx, ALICE, 5_000 * USD);
    // Market A: a long with resting orders on BOTH sides (so the joint max() is live and the
    // Assuming-Price uplift reaches the sell aggregate).
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        3,
        -(300 * USD as i64),
        300 * USD as i64,
        2,
    );
    set_orders(
        &mut ctx,
        ALICE,
        MARKET_A,
        &[entry(1, P100, 2), entry(2, P60, 5)],
        &[entry(3, P100, 4), entry(4, 12_000, 3)],
    );
    // Market B: a SHORT on the fractional grid, under-funded silo, no orders.
    set_position(&mut ctx, ALICE, MARKET_B, -3, 300, 200, 1);

    let a = get_account(&mut ctx, ALICE);
    assert_eq!(
        a.marketIds,
        vec![MARKET_A, MARKET_B],
        "the index names the market set the totals were summed over"
    );

    // ── recompute every total from the per-market view alone ──
    let mut sum_initial = 0u64;
    let mut sum_position_initial = 0u64;
    let mut sum_open_order_initial = 0u64;
    let mut sum_maint = 0u64;
    let mut sum_upnl = 0i64;
    let mut sum_silo = 0i64;
    let mut sum_isolated_margin = 0i64;
    for &m in &a.marketIds {
        let i = margin_info(&mut ctx, ALICE, m);
        sum_initial += i.initialMargin;
        sum_position_initial += i.positionInitialMargin;
        sum_open_order_initial += i.openOrderInitialMargin;
        sum_maint += i.maintMargin;
        sum_upnl += i.unrealizedProfit;
        sum_silo += i.positionMargin;
        sum_isolated_margin += i.isolatedMargin;
    }
    assert!(
        sum_open_order_initial > 0 && sum_upnl != 0 && sum_silo > 0,
        "the fixture must exercise every term, or this proves nothing"
    );

    assert_eq!(a.totalInitialMargin, sum_initial);
    assert_eq!(a.totalPositionInitialMargin, sum_position_initial);
    assert_eq!(a.totalOpenOrderInitialMargin, sum_open_order_initial);
    assert_eq!(a.totalMaintMargin, sum_maint);
    assert_eq!(a.totalUnrealizedProfit, sum_upnl);
    assert_eq!(
        a.totalInitialMargin,
        a.totalPositionInitialMargin + a.totalOpenOrderInitialMargin,
        "totalInitialMargin == PIM + ooIM, the identity Binance's account level relies on"
    );

    // ── the four balance identities ──
    assert_eq!(
        a.totalWalletBalance,
        a.totalCrossWalletBalance + sum_silo,
        "totalWalletBalance = totalCrossWalletBalance + Σ isolatedWallet"
    );
    assert_eq!(
        a.totalMarginBalance,
        a.totalWalletBalance + a.totalUnrealizedProfit,
        "totalMarginBalance = totalWalletBalance + totalUnrealizedProfit (GROSS-based)"
    );
    assert_eq!(
        a.totalMarginBalance,
        a.totalCrossWalletBalance + sum_isolated_margin,
        "…which is also cross + Σ isolatedMargin, reached from the other direction"
    );
    assert_eq!(
        a.availableBalance,
        a.totalCrossWalletBalance - a.totalOpenOrderInitialMargin as i64,
        "availableBalance = totalCrossWalletBalance − totalOpenOrderInitialMargin, unclamped"
    );
    // And `availableBalance` is the admission basis itself, not a parallel reporting number.
    assert_eq!(
        derived_available_balance(&mut ctx, ALICE).unwrap(),
        a.availableBalance as i128,
        "the reported headroom IS the one the engine's gates enforce"
    );
}

/// ONE IMPLEMENTATION, NOT TWO — **the `AccountBalanceChanged` leg.** The event carries the same
/// account-level scalar set `getAccount` returns, and this is the test that fails if they ever
/// diverge for the SAME state.
///
/// Method: take the rich two-market fixture (a long with resting orders on both sides so the joint
/// `max()` and the Assuming-Price uplift are both live, plus a short on a fractional grid with an
/// under-funded silo, at live marks so `PIM`/`maintMargin`/`uPnL` are all non-zero), then force an
/// account write whose net effect on the ledger is ZERO — `credit_perp(0)`. The emitted after-image
/// is therefore an after-image of *exactly* the state `getAccount` is then asked about, so the two
/// must agree on all ten fields with no reasoning about intermediate snapshots at all.
///
/// A zero-delta write is not a path the engine takes; it is the cleanest way to isolate the
/// PRODUCER. `trading::tests::matched_call_emits_a_balance_event_at_each_balance_moving_write`
/// covers the same agreement on a real money-moving call.
#[test]
fn the_event_and_get_account_agree_field_for_field_on_the_same_state() {
    use crate::interface::IPerpDex::AccountBalanceChanged;
    use alloy_sol_types::SolEvent;

    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    add_market(
        &mut ctx,
        MARKET_B,
        B_BASE_DECIMALS,
        B_PRICE_DECIMALS,
        12_345,
    );
    fund(&mut ctx, ALICE, 5_000 * USD);
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        3,
        -(300 * USD as i64),
        300 * USD as i64,
        2,
    );
    set_orders(
        &mut ctx,
        ALICE,
        MARKET_A,
        &[entry(1, P100, 2), entry(2, P60, 5)],
        &[entry(3, P100, 4), entry(4, 12_000, 3)],
    );
    set_position(&mut ctx, ALICE, MARKET_B, -3, 300, 200, 1);

    // A write that moves nothing: the after-image is the state as it stands.
    let _ = JournalTr::take_logs(ctx.journal_mut());
    storage::mutate_account_balance(&mut ctx, ALICE, |a| a.credit_perp(0))
        .unwrap()
        .unwrap();
    let events = JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
        .map(|log| {
            AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    let e = &events[0];

    let a = get_account(&mut ctx, ALICE);
    assert_eq!(
        (
            e.user,
            e.usdcBalance,
            e.totalWalletBalance,
            e.totalCrossWalletBalance,
            e.totalMarginBalance,
            e.totalUnrealizedProfit,
            e.totalInitialMargin,
            e.totalPositionInitialMargin,
            e.totalOpenOrderInitialMargin,
            e.totalMaintMargin,
            e.availableBalance,
        ),
        (
            ALICE,
            a.usdcBalance,
            a.totalWalletBalance,
            a.totalCrossWalletBalance,
            a.totalMarginBalance,
            a.totalUnrealizedProfit,
            a.totalInitialMargin,
            a.totalPositionInitialMargin,
            a.totalOpenOrderInitialMargin,
            a.totalMaintMargin,
            a.availableBalance,
        ),
        "the event and getAccount must be the same numbers — they share one producer, \
         `margin_view::index_account_scalars`, over one market set (the per-user index)"
    );
    // The fixture has to exercise every term, or the equality above proves little.
    assert!(
        e.totalOpenOrderInitialMargin > 0
            && e.totalPositionInitialMargin > 0
            && e.totalMaintMargin > 0
            && e.totalUnrealizedProfit != 0
            && e.totalWalletBalance != e.totalCrossWalletBalance,
        "fixture must make every scalar non-trivial: {:?}",
        (
            e.totalOpenOrderInitialMargin,
            e.totalPositionInitialMargin,
            e.totalMaintMargin,
            e.totalUnrealizedProfit,
            e.totalWalletBalance,
            e.totalCrossWalletBalance
        )
    );
}

/// ONE IMPLEMENTATION, NOT TWO: handed the same market set, the index-driven and list-driven views
/// agree on every shared field, bit for bit.
///
/// If the two ever forked the arithmetic this is the test that catches it. The fields that are NOT
/// shared are the point of each selector: `getAccount` alone reports `totalWalletBalance` /
/// `totalMarginBalance` (they need the WHOLE index to be honest), and `getAccountMargin` alone
/// reports `crossMarginBalance` (the partial-list-safe analogue).
#[test]
fn get_account_and_get_account_margin_agree_on_the_same_market_set() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    add_market(
        &mut ctx,
        MARKET_B,
        B_BASE_DECIMALS,
        B_PRICE_DECIMALS,
        12_345,
    );
    fund(&mut ctx, ALICE, 5_000 * USD);
    place(&mut ctx, ALICE, 0, P100, 4);
    set_position(&mut ctx, ALICE, MARKET_B, -3, 300, 200, 1);

    let idx = get_account(&mut ctx, ALICE);
    let list = account_margin(&mut ctx, ALICE, &idx.marketIds);
    assert_eq!(
        (
            idx.totalCrossWalletBalance,
            idx.totalUnrealizedProfit,
            idx.totalInitialMargin,
            idx.totalPositionInitialMargin,
            idx.totalOpenOrderInitialMargin,
            idx.totalMaintMargin,
            idx.availableBalance,
        ),
        (
            list.totalCrossWalletBalance,
            list.totalUnrealizedProfit,
            list.totalInitialMargin,
            list.totalPositionInitialMargin,
            list.totalOpenOrderInitialMargin,
            list.totalMaintMargin,
            list.availableBalance,
        ),
        "same market set, same walkers ⇒ identical numbers"
    );
    // The one shared-quantity difference is the NAME, and it is the naming bug this change fixed:
    // `crossMarginBalance` is cross + uPnL, whereas the Binance-named `totalMarginBalance` is
    // GROSS + uPnL. They differ by exactly Σ isolatedWallet.
    assert_eq!(
        idx.totalMarginBalance - list.crossMarginBalance,
        idx.totalWalletBalance - idx.totalCrossWalletBalance,
        "the two 'margin balance' figures differ by exactly Σ isolatedWallet"
    );
    assert_ne!(
        idx.totalMarginBalance, list.crossMarginBalance,
        "and on this fixture they really are different numbers, so the names must differ too"
    );
}

/// A market id in the index that names no market is CORRUPT STATE, not a caller mistake, and is
/// reported as an `[INVARIANT]` reject — the same guard the admission-path Σ walk applies to the
/// same set. Contrast `getAccountMargin`, where an unknown id is an ordinary labelled reject.
#[test]
fn get_account_treats_an_index_entry_with_no_market_as_an_invariant_violation() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    // Plant the id by saving a position in a market the market table does not have — the index is
    // maintained by the `save_position` zero-crossing hook, which does not itself validate the id.
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        1,
        -(100 * USD as i64),
        100 * USD as i64,
        1,
    );
    set_position(
        &mut ctx,
        ALICE,
        99,
        1,
        -(100 * USD as i64),
        100 * USD as i64,
        1,
    );
    assert_eq!(
        storage::load_user_markets(&mut ctx, ALICE).unwrap(),
        vec![MARKET_A, 99]
    );
    let input = getAccountCall { user: ALICE }.abi_encode();
    assert_eq!(
        revert_reason(&mut ctx, &input),
        "[INVARIANT] getAccount: user market index holds unknown market 99"
    );
}

// ── The affordability predicate (B1's derived-basis restatement) ───────────

/// REGRESSION (B1), carried over from `UserAccount::has_available_perp`, which this predicate
/// replaced when the escrow was deleted.
///
/// The original bug was `has_available_perp(0)` evaluating `-5 >= 0 == false`, so a NEGATIVE
/// wallet refused a debit of ZERO — locking a distressed user out of exactly the actions that
/// would reduce their risk. The derived basis makes the same trap MORE reachable, not less:
/// `available = wallet − Σ ooIM` can go negative on a mark move alone, with no action by the
/// user. So the exemption has to survive, restated as "a non-positive requirement is always
/// affordable".
#[test]
fn a_non_positive_requirement_is_affordable_at_any_available_including_negative() {
    for available in [i128::MIN, -1_000_000, -5, -1, 0, 1, i128::MAX] {
        for requirement in [i128::MIN, -1_000_000, -1, 0] {
            assert!(
                derived_can_afford(available, requirement),
                "requirement {requirement} must be free at available {available}"
            );
        }
    }
}

/// ...and it opens no hole: every POSITIVE requirement is still refused unless the available
/// covers it in full, with the `>=` boundary unchanged.
#[test]
fn a_positive_requirement_still_needs_the_available_to_cover_it() {
    assert!(!derived_can_afford(-5, 1));
    assert!(!derived_can_afford(0, 1));
    assert!(!derived_can_afford(9, 10));
    assert!(derived_can_afford(10, 10), "the >= boundary");
    assert!(derived_can_afford(11, 10));
    assert!(!derived_can_afford(i128::MIN, 1));
    assert!(derived_can_afford(i128::MAX, i128::MAX));
}

// ── Argument handling ──────────────────────────────────────────────────────

#[test]
fn account_margin_counts_a_duplicate_market_id_once() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    set_position(
        &mut ctx,
        ALICE,
        MARKET_A,
        2,
        -(200 * USD as i64),
        200 * USD as i64,
        1,
    );
    set_orders(&mut ctx, ALICE, MARKET_A, &[], &[entry(1, P100, 5)]);

    let once = account_margin(&mut ctx, ALICE, &[MARKET_A]);
    let thrice = account_margin(&mut ctx, ALICE, &[MARKET_A, MARKET_A, MARKET_A]);
    assert_eq!(as_tuple(&once), as_tuple(&thrice));
    assert_eq!(once.totalOpenOrderInitialMargin, 100 * USD);
}

#[test]
fn views_reject_an_unknown_market() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);

    let input = getMarginInfoCall {
        user: ALICE,
        marketId: 99,
    }
    .abi_encode();
    assert_eq!(
        revert_reason(&mut ctx, &input),
        "getMarginInfo: unknown market"
    );

    let input = getAccountMarginCall {
        user: ALICE,
        marketIds: vec![MARKET_A, 99],
    }
    .abi_encode();
    assert_eq!(
        revert_reason(&mut ctx, &input),
        "getAccountMargin: market 99: getMarginInfo: unknown market"
    );
}

#[test]
fn account_margin_bounds_the_market_id_array() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    let too_many: Vec<u64> = (0..=MAX_MARGIN_INFO_MARKETS as u64).collect();
    let input = getAccountMarginCall {
        user: ALICE,
        marketIds: too_many,
    }
    .abi_encode();
    assert_eq!(
        revert_reason(&mut ctx, &input),
        format!("getAccountMargin: at most {MAX_MARGIN_INFO_MARKETS} market ids")
    );
}

// ── Purity: the whole point is that this layer stores nothing ──────────────

#[test]
fn the_views_write_nothing() {
    let mut ctx = make_ctx();
    setup_a(&mut ctx);
    add_market(
        &mut ctx,
        MARKET_B,
        B_BASE_DECIMALS,
        B_PRICE_DECIMALS,
        12_345,
    );
    fund(&mut ctx, ALICE, 5_000 * USD);
    place(&mut ctx, ALICE, 0, P100, 4);
    set_position(&mut ctx, ALICE, MARKET_B, -3, 300, 200, 1);

    // Snapshot the journal's perp-write counter AFTER all setup, then do only reads.
    let before = PerpHost::perp_write_count(&ctx);
    for _ in 0..3 {
        let _ = margin_info(&mut ctx, ALICE, MARKET_A);
        let _ = margin_info(&mut ctx, ALICE, MARKET_B);
        let _ = account_margin(&mut ctx, ALICE, &[MARKET_A, MARKET_B]);
        // The index-driven roll-up too: it walks positions and order lists, so a `load_*` that is
        // not a `_ref` would enter a key into the block delta and move the commitment from a VIEW.
        let _ = get_account(&mut ctx, ALICE);
    }
    assert_eq!(
        PerpHost::perp_write_count(&ctx),
        before,
        "the derived margin views must not dirty a single key — a write here would move the \
         block commitment"
    );

    // ── ...AND the same fold reached from the EVENT path adds no key either ───────────────────
    //
    // `AccountBalanceChanged` now carries this whole roll-up, so `index_account_scalars` runs on
    // every write that moves a published field — every balance-moving account write, and every order
    // that RESTS (`trading::rest_in_book`, whose admission gate does the fold anyway). If any loader it reaches were not a `_ref`/cache-fill reader it
    // would dirty extra keys, and the perp block commitment — whose input is exactly the block's net
    // key→value delta (`perp_core::compute_block_commitment`) — would move for a reason that has
    // nothing to do with what the call actually changed. This is the mechanical confirmation that the
    // golden commitment cannot shift because of the event: the WRITE contributes its one account key
    // (counted here), the fold behind the log contributes none, and log data is EVM-journaled and
    // never enters the perp delta at all.
    let before_write = PerpHost::perp_write_count(&ctx);
    storage::mutate_account_balance(&mut ctx, ALICE, |a| a.credit_perp(1))
        .unwrap()
        .unwrap();
    assert_eq!(
        PerpHost::perp_write_count(&ctx),
        before_write + 1,
        "the account write dirties exactly ONE key; the event's Σ-over-markets fold behind it adds \
         none. More than one here means a loader on the emit path stopped being a `_ref` reader."
    );
}
