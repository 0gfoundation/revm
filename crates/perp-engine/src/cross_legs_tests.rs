//! Cross margin P4 — the account's cross-leg summary (`types::CrossLegs`, its own per-user blob)
//! and the health check priced from it (`margin_view::cross_health`).
//!
//! The summary's invariant is that it is EXACTLY the state of the account's non-zero positions
//! CROSSED in force, in market order. These tests derive that list independently — from the
//! position blobs, here — and compare it with the stored one after every step of a route-by-route
//! script: taker and maker fills, a partial reduce, a flip, a funding settlement, a batch (the
//! batch working set), a sub-account's order through `asSubAccount`, a fired `closePosition` stop,
//! a switch to ISOLATED and an isolated silo topped up, a cross liquidation episode and its ADL
//! (both legs) — and the one narrow route that moves a leg without `save_position`: a match flush
//! that sends a user who published nothing down the silent route
//! (`save_position_reservation_only_after_funding`) after a ZERO-payment funding settle
//! re-anchored its index.
use alloy_sol_types::{SolCall, SolError, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, Log, U256};

use crate::margin_view::{CrossHealth, CrossHealthLeg};
use crate::{
    interface::IPerpDex::{
        addPositionMarginCall, asSubAccountCall, batchPlaceOrdersCall, createSubAccountCall,
        placeAlgoOrderCall, placeOrderCall, setLeverageCall, setMarginTypeCall,
        updateIndexPriceCall, AccountBalanceChanged, Adl, CrossLiquidationStarted, OrderCancelled,
        PlaceItem, Trade,
    },
    run_perp_dex_call, storage,
    types::{
        derive_sub_account, AccountUpdateReason, CrossLegState, FundingState, MarginTiers, Market,
        UserAccount,
    },
    PerpOutput, PERP_DEX_ADDRESS, USDC_ADDRESS,
};

// ── Fixture ───────────────────────────────────────────────────────────────────

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const USERS: [Address; 3] = [ALICE, BOB, CAROL];

/// Three markets on one grid: base 0 decimals, price 2 decimals (one contract at `P` cents is
/// worth `P · 10^4` quote units), the default 3x tier table, the band off, funding off.
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
const CROSSED: u8 = 0;
const ISOLATED: u8 = 1;
const CLOSE_POSITION: u8 = 0b10;

const fn px(dollars: u64) -> u64 {
    dollars * 100
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
        price_band_bps: 10_000,
        mark_price: px(100),
        tiers: MarginTiers::default(),
        basis_mode: 0,
        deferred_work: 0,
        cumulative_funding_index: 0,
    }
}

/// The real gate. ALICE and CAROL at the default (CROSSED) everywhere, at 3x; BOB, the liquidity,
/// ISOLATED everywhere.
fn make_ctx() -> TestCtx {
    let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
    for a in [USDC_ADDRESS, PERP_DEX_ADDRESS].into_iter().chain(USERS) {
        JournalTr::load_account(ctx.journal_mut(), a).unwrap();
    }
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    for id in MARKETS {
        storage::save_market(&mut ctx, &market(id)).unwrap();
    }
    for u in USERS {
        let a = UserAccount { perp_wallet_balance: WALLET, ..UserAccount::default() };
        storage::save_account(&mut ctx, u, a, AccountUpdateReason::Adjustment).unwrap();
    }
    storage::flush_account_snapshots(&mut ctx).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
        for u in [ALICE, CAROL] {
            ok(&mut ctx, u, &setLeverageCall { marketId: m, leverage: 3 }.abi_encode());
        }
    }
    ctx
}

fn call(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, 30_000_000, caller, U256::ZERO, false, ctx)
        .expect("a perp call must not hard-fail")
}

fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> Vec<Log> {
    let out = call(ctx, caller, input);
    let reason =
        alloy_sol_types::Revert::abi_decode(&out.bytes).map(|r| r.reason).unwrap_or_default();
    assert!(!out.reverted, "call reverted: {reason}");
    JournalTr::take_logs(ctx.journal_mut())
}

fn order_flags(m: u64, side: u8, price: u64, qty: u64, tif: u8, flags: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: m,
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

fn order(m: u64, side: u8, price: u64, qty: u64, tif: u8) -> Vec<u8> {
    order_flags(m, side, price, qty, tif, 0)
}

fn set_type(ctx: &mut TestCtx, user: Address, m: u64, t: u8) {
    ok(ctx, user, &setMarginTypeCall { marketId: m, marginType: t }.abi_encode());
}

fn set_wallet(ctx: &mut TestCtx, user: Address, w: i64) {
    storage::mutate_account(ctx, user, |a| a.perp_wallet_balance = w).unwrap();
}

fn pos(ctx: &mut TestCtx, user: Address, m: u64) -> crate::types::PerpPosition {
    storage::load_position(ctx, user, m).unwrap()
}

fn evs<E: SolEvent>(logs: &[Log]) -> Vec<E> {
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|l| E::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .collect()
}

/// An oracle tick one interval after the market's last; the last traded price is reset first so
/// the mark is exactly the index (funding off).
fn tick(ctx: &mut TestCtx, m: u64, index: u64) -> Vec<Log> {
    let ts = storage::load_index_price_state(ctx, m).unwrap().timestamp + 15;
    storage::save_last_traded_price(ctx, m, 0).unwrap();
    let logs = ok(
        ctx,
        ADMIN,
        &updateIndexPriceCall { marketId: m, indexPrice: index, timestamp: ts }.abi_encode(),
    );
    assert_eq!(storage::load_mark_price(ctx, m).unwrap(), index, "the tick set the mark");
    logs
}

/// `user`'s cross legs derived HERE, from the position blobs: one entry per market whose position
/// stores CROSSED and is non-zero (the real gate is on), in market order.
fn walked_legs(ctx: &mut TestCtx, user: Address) -> Vec<CrossLegState> {
    let mut legs = Vec::new();
    for m in MARKETS {
        let p = pos(ctx, user, m);
        if p.margin_type == CROSSED && p.amount != 0 {
            legs.push(CrossLegState {
                market_id: m,
                amount: p.amount,
                v_quote_balance: p.v_quote_balance,
                last_funding_index: p.last_funding_index,
            });
        }
    }
    legs
}

fn stored_legs(ctx: &mut TestCtx, user: Address) -> Vec<CrossLegState> {
    storage::load_cross_legs_ref(ctx, user).unwrap().0.clone()
}

/// The invariant, for every account in `users`.
fn assert_legs(ctx: &mut TestCtx, users: &[Address], at: &str) {
    for &u in users {
        assert_eq!(stored_legs(ctx, u), walked_legs(ctx, u), "{at}: {u}'s cross legs");
    }
}

fn markets_of(legs: &[CrossLegState]) -> Vec<u64> {
    legs.iter().map(|l| l.market_id).collect()
}

// ── D1: the summary follows every route ────────────────────────────────────────

/// One account through every route that writes a position, the invariant re-checked after each.
#[test]
fn the_legs_follow_every_route_that_moves_a_cross_position() {
    let mut ctx = make_ctx();
    let sub = derive_sub_account(ALICE, 0);
    let all = [ALICE, BOB, CAROL, sub];
    assert!(stored_legs(&mut ctx, ALICE).is_empty(), "no position, no leg");

    // A taker fill opens a CROSSED long — one leg.
    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 10, GTC));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 10, IOC));
    assert_legs(&mut ctx, &all, "taker open");
    assert_eq!(markets_of(&stored_legs(&mut ctx, ALICE)), vec![M1]);

    // A maker fill opens a CROSSED short on M2 — a second leg, in market order.
    ok(&mut ctx, ALICE, &order(M2, SELL, px(100), 5, GTC));
    ok(&mut ctx, BOB, &order(M2, BUY, px(100), 5, IOC));
    assert_legs(&mut ctx, &all, "maker open");
    assert_eq!(markets_of(&stored_legs(&mut ctx, ALICE)), vec![M1, M2]);

    // A partial reduce moves the leg's amount and vQuote.
    ok(&mut ctx, BOB, &order(M1, BUY, px(101), 4, GTC));
    ok(&mut ctx, ALICE, &order(M1, SELL, px(101), 4, IOC));
    assert_legs(&mut ctx, &all, "partial reduce");
    assert_eq!(stored_legs(&mut ctx, ALICE)[0].amount, 6);

    // A flip: short 5 → long 3 on M2.
    ok(&mut ctx, BOB, &order(M2, SELL, px(99), 8, GTC));
    ok(&mut ctx, ALICE, &order(M2, BUY, px(99), 8, IOC));
    assert_legs(&mut ctx, &all, "flip");
    assert_eq!(stored_legs(&mut ctx, ALICE)[1].amount, 3);

    // A funding settlement with a payment re-anchors the index (and moves the cross wallet): the
    // index moves, the next fill settles it.
    let fs =
        FundingState { last_funding_rate: 0, next_funding_ts: 0, cumulative_funding_index: 7_000 };
    storage::save_funding_state(&mut ctx, M1, &fs).unwrap();
    let w0 = storage::load_account(&mut ctx, ALICE).unwrap().perp_wallet_balance;
    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 1, GTC));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 1, IOC));
    assert_ne!(storage::load_account(&mut ctx, ALICE).unwrap().perp_wallet_balance, w0);
    assert_eq!(pos(&mut ctx, ALICE, M1).last_funding_index, 7_000);
    assert_legs(&mut ctx, &all, "funding settle");

    // A batch (the batch working set holds ALICE's account and positions): one item fills on M3,
    // one rests far below the book on M1 (the episode at the end cancels it).
    ok(&mut ctx, BOB, &order(M3, SELL, px(100), 2, GTC));
    let item = |m, price, qty, tif| PlaceItem {
        marketId: m,
        side: BUY,
        price,
        quantity: qty,
        orderType: 0,
        tif,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    };
    let batch = batchPlaceOrdersCall {
        orders: vec![item(M3, px(100), 2, IOC), item(M1, px(50), 1, GTC)],
    };
    ok(&mut ctx, ALICE, &batch.abi_encode());
    assert_legs(&mut ctx, &all, "batch");
    assert_eq!(markets_of(&stored_legs(&mut ctx, ALICE)), vec![M1, M2, M3]);

    // A sub-account's order, placed by its master through `asSubAccount`.
    ok(&mut ctx, ALICE, &createSubAccountCall { name: "desk".into() }.abi_encode());
    set_wallet(&mut ctx, sub, 1_000 * USD);
    ok(&mut ctx, BOB, &order(M2, SELL, px(100), 1, GTC));
    let inner = order(M2, BUY, px(100), 1, IOC);
    ok(&mut ctx, ALICE, &asSubAccountCall { subAccount: sub, data: inner.into() }.abi_encode());
    assert_legs(&mut ctx, &all, "asSubAccount");
    assert_eq!(markets_of(&stored_legs(&mut ctx, sub)), vec![M2]);

    // A fired closePosition stop closes M3 against BOB's bid: its leg goes.
    ok(&mut ctx, BOB, &order(M3, BUY, px(80), 2, GTC));
    let stop = placeAlgoOrderCall {
        algoType: 0,
        marketId: M3,
        side: SELL,
        orderType: 1,
        triggerPrice: px(95),
        price: 0,
        quantity: 0,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags: CLOSE_POSITION,
    };
    ok(&mut ctx, ALICE, &stop.abi_encode());
    tick(&mut ctx, M3, px(90));
    assert_eq!(pos(&mut ctx, ALICE, M3).amount, 0, "the stop fired and closed M3");
    assert_legs(&mut ctx, &all, "closePosition stop");
    assert_eq!(markets_of(&stored_legs(&mut ctx, ALICE)), vec![M1, M2]);

    // Flat M3 switched to ISOLATED: an isolated position has no leg, and topping up its silo moves
    // nothing in the list.
    set_type(&mut ctx, ALICE, M3, ISOLATED);
    ok(&mut ctx, BOB, &order(M3, SELL, px(90), 1, GTC));
    ok(&mut ctx, ALICE, &order(M3, BUY, px(90), 1, IOC));
    let before = stored_legs(&mut ctx, ALICE);
    ok(&mut ctx, ALICE, &addPositionMarginCall { marketId: M3, amount: USD as u64 }.abi_encode());
    assert_eq!(stored_legs(&mut ctx, ALICE), before);
    assert_legs(&mut ctx, &all, "isolated open + addPositionMargin");
    assert_eq!(markets_of(&before), vec![M1, M2]);

    // A cross liquidation episode, its rest ADL-ed against a CROSSED winner: CAROL short on M1.
    ok(&mut ctx, BOB, &order(M1, BUY, px(100), 3, GTC));
    ok(&mut ctx, CAROL, &order(M1, SELL, px(100), 3, IOC));
    assert_legs(&mut ctx, &all, "CAROL's short");
    set_wallet(&mut ctx, ALICE, 0);
    let logs = tick(&mut ctx, M1, px(60));
    assert_eq!(evs::<CrossLiquidationStarted>(&logs).len(), 1, "ALICE's episode");
    assert!(
        evs::<Adl>(&logs).iter().any(|a| a.adlUser == CAROL),
        "the rest went to ADL against CAROL"
    );
    assert_legs(&mut ctx, &all, "episode + ADL");
}

/// The one route that moves a cross leg WITHOUT `save_position`: a match in which the user only
/// LOST an order (a CROSSED maker refused for want of cash at hand, `MakerInsolvent`) flushes it
/// through `save_position_reservation_only_after_funding` — nothing published, `amount`, `margin`
/// and the account unchanged — after `get_or_load` re-anchored its funding index with a payment
/// that truncates to zero. The leg carries the new index.
#[test]
fn a_silent_match_flush_carries_a_zero_payment_funding_re_anchor_into_the_leg() {
    let mut ctx = make_ctx();
    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 10, GTC));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 10, IOC));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(99), 1, GTC));
    set_wallet(&mut ctx, ALICE, 0);
    // `10 · 5 / 10^2` truncates to a zero payment.
    let fs = FundingState { last_funding_rate: 0, next_funding_ts: 0, cumulative_funding_index: 5 };
    storage::save_funding_state(&mut ctx, M1, &fs).unwrap();
    assert_eq!(stored_legs(&mut ctx, ALICE)[0].last_funding_index, 0);

    let logs = ok(&mut ctx, BOB, &order(M1, SELL, px(99), 1, IOC));
    assert!(evs::<Trade>(&logs).is_empty(), "the opening maker fill was refused");
    let cancelled = evs::<OrderCancelled>(&logs);
    assert!(cancelled.iter().any(|c| c.user == ALICE), "ALICE's bid was cancelled");
    assert!(
        evs::<AccountBalanceChanged>(&logs).iter().all(|r| r.user != ALICE),
        "nothing published for ALICE: the silent route"
    );
    let p = pos(&mut ctx, ALICE, M1);
    assert_eq!((p.amount, p.last_funding_index), (10, 5), "re-anchored, nothing paid");
    assert_eq!(storage::load_account(&mut ctx, ALICE).unwrap().perp_wallet_balance, 0);
    assert_eq!(stored_legs(&mut ctx, ALICE)[0].last_funding_index, 5, "the leg carries the index");
    assert_legs(&mut ctx, &USERS, "silent flush");
}

/// `save_position` sets the leg BEFORE it writes the position: a write that fails AFTER the
/// position write — here the per-user index's cap, which no call reaches (the placement gate
/// refuses a 17th market first), so the full index is planted — keeps both or neither. Inside a
/// batch it matters: an aborted item's writes are kept (`drive_batch` flushes the working set
/// either way), and a position stored without its leg would be invisible to the sweep, ADL and
/// the flag's end, which judge an account by its legs alone.
#[test]
fn a_position_write_that_fails_after_storing_keeps_its_leg() {
    use crate::host::PerpHost as _;
    let mut ctx = make_ctx();
    ctx.live().set_user_markets(ALICE, (10..10 + crate::types::MAX_USER_MARKETS as u64).collect());
    storage::begin_batch_ws(&mut ctx, ALICE);
    let mut p = pos(&mut ctx, ALICE, M1);
    assert_eq!((p.margin_type, p.amount), (CROSSED, 0));
    p.amount = 5;
    p.v_quote_balance = -5 * px(100) as i64 * 10_000;
    let r = storage::save_position(&mut ctx, ALICE, M1, &p, AccountUpdateReason::Order);
    let err = format!("{:?}", r.expect_err("the per-user index is full"));
    assert!(err.contains("[INVARIANT]") && err.contains("markets"), "{err}");
    storage::flush_batch_ws(&mut ctx);

    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 5, "the position write was kept");
    assert_eq!(
        stored_legs(&mut ctx, ALICE),
        walked_legs(&mut ctx, ALICE),
        "…and so was its leg"
    );
    assert_eq!(markets_of(&stored_legs(&mut ctx, ALICE)), vec![M1]);
}

/// The aggregates-only route (an order resting, a cancel) writes the four per-side aggregates onto
/// the STORED position and nothing else, so even a caller holding a STALE copy — read before a
/// fill moved `amount` / `v_quote_balance` — cannot roll the position back under its leg. Its
/// signature makes that so in every build: it takes `SideAggregates`, not a position.
#[test]
fn the_aggregates_only_route_cannot_write_back_a_stale_position() {
    let mut ctx = make_ctx();
    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 10, GTC));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 4, IOC));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(99), 2, GTC));
    let stale = pos(&mut ctx, ALICE, M1);
    assert_eq!((stale.amount, stale.total_buy_qty), (4, 2));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 3, IOC));
    let fresh = pos(&mut ctx, ALICE, M1);
    assert_eq!(fresh.amount, 7, "the fill moved the stored position past the stale copy");

    let mut sides = storage::SideAggregates::of(&stale);
    sides.sell_qty = 1;
    storage::save_position_reservation_only(&mut ctx, ALICE, M1, sides).unwrap();
    let after = pos(&mut ctx, ALICE, M1);
    assert_eq!(storage::SideAggregates::of(&after), sides, "the aggregates were written");
    let rest = |p: &crate::types::PerpPosition| {
        let mut p = p.clone();
        p.total_buy_qty = 0;
        p.total_buy_notional = 0;
        p.total_sell_qty = 0;
        p.total_sell_notional = 0;
        p
    };
    assert_eq!(rest(&after), rest(&fresh), "…and nothing else: the stored fields stand");
    assert_legs(&mut ctx, &USERS, "aggregates-only write");
    assert_eq!(stored_legs(&mut ctx, ALICE)[0].amount, 7);
}

// ── The call's health memo (ADL ranking) ──────────────────────────────────────

/// ADL's memoized read (`cross_health_for_adl`) answers again only while every input of the health
/// check holds — the stored wallet, the stored legs, every `Market` — and only within the call:
/// each step below moves one input and must be re-priced (a miss) to the fresh check's numbers; an
/// untouched account is a hit with the same numbers, for the market asked before or another one.
/// Compared with a fresh `cross_health` here as well as by the memo's own `debug_assertions`
/// re-check, so this holds in a release test build too.
#[test]
fn the_health_memo_answers_again_only_while_its_inputs_hold() {
    use crate::margin_view::{cross_health, cross_health_for_adl, memo_stats};
    let mut ctx = make_ctx();
    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 10, GTC));
    ok(&mut ctx, BOB, &order(M2, BUY, px(100), 10, GTC));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 4, IOC));
    ok(&mut ctx, ALICE, &order(M2, SELL, px(100), 3, IOC));
    let step = |ctx: &mut TestCtx, m: u64, at: &str, want_hit: bool| {
        memo_stats::take();
        let memo = cross_health_for_adl(ctx, ALICE, m, true, &mut Vec::new()).unwrap();
        let (hits, misses) = memo_stats::take();
        assert_eq!((hits, misses), (want_hit as u64, !want_hit as u64), "{at}: hit / miss");
        let mut legs = Vec::new();
        let fresh = cross_health(ctx, ALICE, Some(&mut legs)).unwrap();
        let leg = legs.iter().find(|l| l.market_id == m).copied();
        assert_eq!(memo, (fresh, leg), "{at}: the fresh check's numbers");
        memo.0
    };
    let first = step(&mut ctx, M1, "first", false);
    assert!(first.has_cross);
    assert_eq!(step(&mut ctx, M1, "untouched", true), first);
    assert_eq!(step(&mut ctx, M2, "untouched, another market's leg", true), first);
    assert_eq!(step(&mut ctx, M3, "untouched, a market with no leg", true), first);

    // The wallet moves, nothing else.
    set_wallet(&mut ctx, ALICE, WALLET - 7);
    let after_wallet = step(&mut ctx, M1, "wallet", false);
    assert_eq!(after_wallet.e, first.e - 7);
    step(&mut ctx, M1, "wallet, again", true);

    // A leg moves, the wallet does not: a position write through the one door.
    let mut p = pos(&mut ctx, ALICE, M2);
    p.amount -= 1;
    p.v_quote_balance += px(100) as i64 * 10_000;
    storage::save_position(&mut ctx, ALICE, M2, &p, AccountUpdateReason::Order).unwrap();
    let after_leg = step(&mut ctx, M2, "leg", false);
    assert_ne!(after_leg.cross_mm, after_wallet.cross_mm, "the leg is priced again");
    step(&mut ctx, M2, "leg, again", true);

    // A mark moves, nothing of ALICE's does.
    storage::save_mark_price(&mut ctx, M2, px(90)).unwrap();
    let after_mark = step(&mut ctx, M2, "mark", false);
    assert_ne!(after_mark.e, after_leg.e, "the short leg gained");
    step(&mut ctx, M1, "mark, again", true);

    // A new call drops the memo.
    storage::begin_perp_call(&mut ctx);
    assert_eq!(step(&mut ctx, M1, "next call", false), after_mark);
}

/// One oracle tick that liquidates several bankrupt cross accounts, each ADL'd (empty book)
/// against the same cross holders, ends in the same store — the same block delta — and the same
/// log stream whether ADL ranks the holders through the call's memo or re-prices every one of them
/// for every loser; and the memo did answer for the later losers.
#[test]
fn a_multi_loser_adl_tick_is_the_same_with_and_without_the_memo() {
    use crate::host::InMemoryHost;
    use crate::margin_view::memo_stats;
    const T0: u64 = 1_000_005;
    const Q: u64 = 10;
    let winners: Vec<Address> = (1..=6u8).map(|i| Address::repeat_byte(0x40 + i)).collect();
    let losers: Vec<Address> = (1..=3u8).map(|i| Address::repeat_byte(0x70 + i)).collect();
    let run = |h: &mut InMemoryHost, who: Address, input: &[u8]| {
        let out = run_perp_dex_call(input, 30_000_000, who, U256::ZERO, false, h).unwrap();
        let reason =
            alloy_sol_types::Revert::abi_decode(&out.bytes).map(|r| r.reason).unwrap_or_default();
        assert!(!out.reverted, "call reverted: {reason}");
    };
    let wallet = |h: &mut InMemoryHost, u: Address, w: i64| {
        storage::mutate_account(h, u, |a| a.perp_wallet_balance = w).unwrap();
    };
    let tick_at = |h: &mut InMemoryHost, m: u64, index: u64, ts: u64| {
        storage::save_last_traded_price(h, m, 0).unwrap();
        let tick = updateIndexPriceCall { marketId: m, indexPrice: index, timestamp: ts };
        run(h, ADMIN, &tick.abi_encode());
    };
    // The world: BOB (ISOLATED) makes the markets; every winner shorts market 1 and goes long 2
    // and 3; every loser goes long market 1 and is left with 20% of its notional.
    let mut world = InMemoryHost::new(T0);
    storage::save_admin(&mut world, ADMIN).unwrap();
    for m in MARKETS {
        storage::save_market(&mut world, &market(m)).unwrap();
    }
    storage::save_insurance_fund(&mut world, 1_000_000_000 * USD as u64).unwrap();
    wallet(&mut world, BOB, WALLET);
    for m in MARKETS {
        run(&mut world, BOB, &setMarginTypeCall { marketId: m, marginType: ISOLATED }.abi_encode());
    }
    let n_w = winners.len() as u64;
    run(&mut world, BOB, &order(M1, BUY, px(100), n_w * Q, GTC));
    run(&mut world, BOB, &order(M2, SELL, px(100), n_w * Q, GTC));
    run(&mut world, BOB, &order(M3, SELL, px(100), n_w * Q, GTC));
    for (i, &w) in winners.iter().enumerate() {
        wallet(&mut world, w, (20_000 + 3_000 * i as i64) * USD);
        run(&mut world, w, &order(M1, SELL, px(100), Q, IOC));
        run(&mut world, w, &order(M2, BUY, px(100), Q, IOC));
        run(&mut world, w, &order(M3, BUY, px(100), Q, IOC));
    }
    run(&mut world, BOB, &order(M1, SELL, px(100), losers.len() as u64 * Q, GTC));
    for &u in &losers {
        wallet(&mut world, u, WALLET);
        run(&mut world, u, &order(M1, BUY, px(100), Q, IOC));
        wallet(&mut world, u, (Q * px(100) * 10_000) as i64 / 5);
    }
    for m in MARKETS {
        tick_at(&mut world, m, px(100), T0);
    }
    world.end_block();

    let outcome = |bypass: bool| {
        let mut h = InMemoryHost::new(world.now);
        h.committed = world.committed.clone();
        h.balances = world.balances.clone();
        h.logs.clear();
        memo_stats::bypass(bypass);
        memo_stats::take();
        tick_at(&mut h, M1, px(78), T0 + 15);
        memo_stats::bypass(false);
        let stats = memo_stats::take();
        let logs = std::mem::take(&mut h.logs);
        let commitment = perp_core::compute_block_commitment(U256::ZERO, &h.end_block());
        (logs, commitment, stats)
    };
    let (logs, commitment, (hits, misses)) = outcome(false);
    let (logs_fresh, commitment_fresh, fresh_stats) = outcome(true);
    assert_eq!(fresh_stats, (0, 0), "the bypass re-priced every holder");
    let liquidations = evs::<crate::interface::IPerpDex::Liquidation>(&logs).len();
    assert_eq!(liquidations, losers.len(), "every loser was liquidated");
    assert!(evs::<Adl>(&logs).len() >= losers.len(), "…and ADL'd");
    // The first loser's ranking keeps no memo; the second prices the five holders left and keeps
    // them; the third finds the four no fill touched.
    assert_eq!((hits, misses), (4, 5), "the later losers' rankings hit the memo");
    assert_eq!(logs, logs_fresh, "the same log stream");
    assert_eq!(commitment, commitment_fresh, "the same block delta");
}

// ── D3 / D4: the health check is the walk's numbers ─────────────────────────────

/// `cross_health` against `cross_fold` computed HERE — not through the `debug_assertions`
/// cross-check inside it, so this holds in a release test build too: `W`, `E`, `Σ_C MM`,
/// `has_cross`, and every leg the walk prices for a position CROSSED in force and non-zero.
fn assert_health_is_the_fold(ctx: &mut TestCtx, user: Address, at: &str) -> CrossHealth {
    let mut legs = Vec::new();
    let health = crate::margin_view::cross_health(ctx, user, Some(&mut legs)).unwrap();
    let mut walked = Vec::new();
    let fold = crate::margin_view::cross_fold(ctx, user, None, None, Some(&mut walked)).unwrap();
    assert_eq!(
        (health.w, health.e, health.cross_mm, health.has_cross),
        (fold.w, fold.e, fold.cross_mm, fold.has_cross),
        "{at}: {user}'s health against the walk"
    );
    let fold_legs: Vec<CrossHealthLeg> = walked
        .iter()
        .filter(|l| l.cross && l.has_position)
        .map(|l| CrossHealthLeg {
            market_id: l.market_id,
            upnl: l.upnl,
            mm: l.mm,
            pending_funding: l.pending_funding,
        })
        .collect();
    assert_eq!(legs, fold_legs, "{at}: {user}'s legs against the walk");
    health
}

/// An account with three cross legs (one priced at a funding index that moved, one short) and an
/// isolated position beside them: the health check reports the walk's numbers through opens, a
/// funding move, a mark move, a partial close and a close — and an account with no cross position
/// reports `E = W`, `Σ MM = 0`, no leg.
#[test]
fn the_health_check_reports_the_walks_numbers() {
    let mut ctx = make_ctx();
    let h = assert_health_is_the_fold(&mut ctx, ALICE, "no position");
    assert_eq!((h.e, h.cross_mm, h.has_cross), (WALLET as i128, 0, false));

    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 10, GTC));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 10, IOC));
    ok(&mut ctx, BOB, &order(M2, BUY, px(100), 4, GTC));
    ok(&mut ctx, ALICE, &order(M2, SELL, px(100), 4, IOC));
    set_type(&mut ctx, ALICE, M3, ISOLATED);
    ok(&mut ctx, BOB, &order(M3, SELL, px(100), 2, GTC));
    ok(&mut ctx, ALICE, &order(M3, BUY, px(100), 2, IOC));
    let h = assert_health_is_the_fold(&mut ctx, ALICE, "opens");
    assert!(h.has_cross && h.cross_mm > 0);

    let fs =
        FundingState { last_funding_rate: 0, next_funding_ts: 0, cumulative_funding_index: 3_333 };
    storage::save_funding_state(&mut ctx, M1, &fs).unwrap();
    let mut legs = Vec::new();
    crate::margin_view::cross_health(&mut ctx, ALICE, Some(&mut legs)).unwrap();
    assert_ne!(legs[0].pending_funding, 0, "M1's leg owes funding");
    assert_health_is_the_fold(&mut ctx, ALICE, "funding moved");

    storage::save_mark_price(&mut ctx, M2, px(130)).unwrap();
    assert_health_is_the_fold(&mut ctx, ALICE, "mark moved");
    ok(&mut ctx, BOB, &order(M1, BUY, px(100), 4, GTC));
    ok(&mut ctx, ALICE, &order(M1, SELL, px(100), 4, IOC));
    assert_health_is_the_fold(&mut ctx, ALICE, "partial close");
    ok(&mut ctx, BOB, &order(M2, SELL, px(130), 4, GTC));
    ok(&mut ctx, ALICE, &order(M2, BUY, px(130), 4, IOC));
    let h = assert_health_is_the_fold(&mut ctx, ALICE, "close");
    let mut legs = Vec::new();
    crate::margin_view::cross_health(&mut ctx, ALICE, Some(&mut legs)).unwrap();
    assert_eq!(legs.iter().map(|l| l.market_id).collect::<Vec<_>>(), vec![M1]);
    assert_eq!(h.faulted_legs, 0);
    for u in USERS {
        assert_health_is_the_fold(&mut ctx, u, "every account");
    }
}

/// J4, the TOTAL policy: a leg priced at a mark of `0` counts `uPnL = MM = 0` and stays a leg; a
/// leg whose pending funding leaves `i64` is left OUT of every sum and counted — exactly as the
/// walk does, which leaves the same leg out.
#[test]
fn a_zero_mark_and_an_unpriceable_leg_are_the_walks_too() {
    let mut ctx = make_ctx();
    for m in MARKETS {
        ok(&mut ctx, BOB, &order(m, SELL, px(100), 1, GTC));
        ok(&mut ctx, ALICE, &order(m, BUY, px(100), 1, IOC));
    }
    storage::save_mark_price(&mut ctx, M3, 0).unwrap();
    let fs = FundingState {
        last_funding_rate: 0,
        next_funding_ts: 0,
        cumulative_funding_index: 1_000_000_000_000_000_000_000,
    };
    storage::save_funding_state(&mut ctx, M2, &fs).unwrap();
    let h = assert_health_is_the_fold(&mut ctx, ALICE, "J4");
    assert_eq!(h.faulted_legs, 1, "M2's leg cannot be priced");
    let fold = crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, None).unwrap();
    assert_eq!(fold.faulted_legs, 1);
    let mut legs = Vec::new();
    crate::margin_view::cross_health(&mut ctx, ALICE, Some(&mut legs)).unwrap();
    let m3 = legs.iter().find(|l| l.market_id == M3).copied().unwrap();
    assert_eq!((m3.upnl, m3.mm), (0, 0), "no price: neither props the account up nor drags it");
    assert!(legs.iter().all(|l| l.market_id != M2));
    assert!(h.has_cross);
}

/// With the gate off (a test's override — the engine before the flip) no position is CROSSED in
/// force, whatever the list holds: no leg is read, `E = W`, `Σ MM = 0`, as the walk reports.
#[test]
fn gated_off_the_health_check_sees_no_cross_leg() {
    let mut ctx = make_ctx();
    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 10, GTC));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 10, IOC));
    assert_eq!(stored_legs(&mut ctx, ALICE).len(), 1);
    let _off = crate::margin_type::test_gate::disable();
    let h = assert_health_is_the_fold(&mut ctx, ALICE, "gated off");
    let w = storage::load_account(&mut ctx, ALICE).unwrap().perp_wallet_balance as i128;
    assert_eq!((h.e, h.cross_mm, h.has_cross, h.faulted_legs), (w, 0, false, 0));
}

/// **D4** — a leg list that has drifted from the positions is caught by the walk every health
/// check re-runs in a debug build: a planted leg of the wrong size. (A release build does no walk
/// and prices what the list says — the drift the debug check exists to catch first.)
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "diverged from the CROSSED non-zero"))]
fn a_drifted_leg_list_is_caught_by_the_debug_walk() {
    let mut ctx = make_ctx();
    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 10, GTC));
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 10, IOC));
    let mut leg = stored_legs(&mut ctx, ALICE)[0];
    leg.amount = 11;
    storage::plant_cross_leg_for_test(&mut ctx, ALICE, M1, Some(leg)).unwrap();
    let h = crate::margin_view::cross_health(&mut ctx, ALICE, None).unwrap();
    let fold = crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, None).unwrap();
    assert_ne!(h.cross_mm, fold.cross_mm, "release: priced from the drifted list");
}

/// **D4** — the health numbers themselves are held to the walk's: a leg naming a market that does
/// not exist (corrupt state) is left out and counted, and a debug build stops on it first.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "names unknown market 99"))]
fn a_leg_naming_no_market_is_left_out_and_flagged() {
    let mut ctx = make_ctx();
    let leg = CrossLegState {
        market_id: 99,
        amount: 1,
        v_quote_balance: -100,
        last_funding_index: 0,
    };
    storage::plant_cross_leg_for_test(&mut ctx, ALICE, 99, Some(leg)).unwrap();
    let h = crate::margin_view::cross_health(&mut ctx, ALICE, None).unwrap();
    assert_eq!((h.faulted_legs, h.has_cross, h.cross_mm), (1, false, 0));
}
