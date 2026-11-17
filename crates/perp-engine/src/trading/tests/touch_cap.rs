//! **Q1: the per-taker maker-touch cap** (`MAX_TAKER_MAKER_TOUCHES` = 64), written from the spec
//! (`q1_spec.md` §7), with every expected value derived by hand from the book each test builds.
//!
//! What counts as a touch: every LIVE maker order the walk loads — filled, STP-cancelled or STP-
//! stopped at, or cancelled by the maker K9 / frozen-account guard. Stale lazy-queue ids do not
//! count. At the 65th live maker a submitted GTC / FOK is REJECTED write-clean; an IOC / market
//! order fills up to the cap and expires the rest (`OrderExpired`, reason 0).
//!
//! Every call goes through the full shell (`run_perp_dex_call`), so the commit-only tripwire (a
//! `debug_assert` on any write-then-revert) is in the loop; every reject is also asserted
//! write-free, log-free and world-identical ([`rejected`]). Every walking test runs for BOTH taker
//! sides ([`DIRS`]) and, where the spec says so, for one level of 65 makers and for 65 levels of one
//! maker each ([`LAYOUTS`]). Makers are single-lot throughout.
//!
//! The fixture is a trimmed copy of `stp_spec`'s (itself self-contained), with levels one TICK apart
//! so 65+ levels fit inside the 10% fill-time band.

use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use ed25519_dalek::{Signer, SigningKey};
use primitives::{address, hardfork::SpecId, Address, FixedBytes, U256};

use crate::call::PerpOutput;
use crate::host::PerpHost;
use crate::interface::IPerpDex::{
    batchPlaceOrdersCall, batchPlaceOrdersSignedCall, cancelOrderCall, placeOrderCall,
    placeOrderSignedCall, registerApiKeyCall, OrderCancelled, OrderExpired, OrderRested, PlaceItem,
    Trade,
};
use crate::trading::{MAX_TAKER_MAKER_TOUCHES, TOUCH_CAP_REJECT};
use crate::types::{AccountUpdateReason, MarginTiers, Market};
use crate::{run_perp_dex_call, storage, PERP_DEX_ADDRESS, USDC_ADDRESS};

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
const DAVE: Address = address!("4444444444444444444444444444444444444444");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const MARKET_ID: u64 = 1;
/// $100.00 at price_decimals 2; the default 10% band admits 9_000..=11_000.
const MARK: u64 = 10_000;
const WALLET: i64 = 1_000_000_000_000_000;
const GAS: u64 = 30_000_000;

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const MKT: u8 = 1;
const GTC: u8 = 0;
const IOC: u8 = 1;
const FOK: u8 = 2;

/// STP modes (`flags` bits 3:2).
const EM: u8 = 0b0000;
const ET: u8 = 0b0100;

/// `CancelReason`.
const C_MAKER_INSOLVENT: u8 = 2;
const C_LIQUIDATION: u8 = 3;
const C_STP: u8 = 9;
/// `OrderExpired.reason`.
const X_TIF: u8 = 0;
const X_STP: u8 = 9;

const CAP: usize = 64;

/// Batch status tags and reasons.
const TAG_REJECTED: u8 = 0;
const TAG_ACCEPTED: u8 = 1;
const TAG_TERMINAL: u8 = 2;
const R_NONE: u8 = 0;
const R_OTHER: u8 = 255;

const E_FOK: &str = "placeOrder: FOK order cannot be fully filled";
const E_DUP_SIG: &str = "placeOrderSigned: duplicate signature (already submitted)";

const SIGNED_TS: u64 = 1;
const SIGNED_RECV: u64 = 60;

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    crate::margin::margin_type::test_gate::isolated_by_default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, BOB, CAROL, DAVE, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

/// Market 1 at a $100 mark (base 0 / price 2 decimals, tick 1, step 1, no fees, no funding) and a
/// $1e15 perp wallet for ALICE, BOB, CAROL and DAVE. Incidental logs discarded.
fn fresh() -> TestCtx {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    storage::save_market(
        &mut ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 0,
            price_decimals: 2,
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
    storage::save_mark_price(&mut ctx, MARKET_ID, MARK).unwrap();
    for u in [ALICE, BOB, CAROL, DAVE] {
        set_wallet(&mut ctx, u, WALLET);
    }
    let _ = take_logs(&mut ctx);
    ctx
}

fn set_wallet(ctx: &mut TestCtx, user: Address, perp: i64) {
    let mut a = storage::load_account(ctx, user).unwrap();
    a.perp_wallet_balance = perp;
    storage::save_account(ctx, user, a, AccountUpdateReason::Adjustment).unwrap();
}

/// Sets the cross-liquidation freeze bit (`liq_flags` bit 0) on `user`'s account.
fn freeze(ctx: &mut TestCtx, user: Address) {
    storage::set_in_liquidation_for_test(ctx, user, true).unwrap();
    assert!(storage::load_account(ctx, user).unwrap().in_liquidation());
}

// ── Direction and book layout ────────────────────────────────────────────────────────────────

/// A walk direction, named by the TAKER's side.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Dir(u8);

const DIRS: [Dir; 2] = [Dir(BUY), Dir(SELL)];

impl Dir {
    fn taker(self) -> u8 {
        self.0
    }
    fn maker(self) -> u8 {
        1 - self.0
    }
    /// Maker level `k` from the touch, ONE tick per level: asks `10_000, 10_001, …` for a buy
    /// taker, bids `10_000, 9_999, …` for a sell taker. In band for `k <= 1_000`.
    fn lvl(self, k: usize) -> u64 {
        if self.0 == BUY {
            MARK + k as u64
        } else {
            MARK - k as u64
        }
    }
    /// A taker limit that crosses every maker level the tests place.
    fn through(self) -> u64 {
        self.lvl(500)
    }
    /// The taker's signed position change per unit filled.
    fn sign(self) -> i64 {
        if self.0 == BUY {
            1
        } else {
            -1
        }
    }
}

/// Where maker `k` rests: all at the touch, or one per level.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    OneLevel,
    Ladder,
}

const LAYOUTS: [Layout; 2] = [Layout::OneLevel, Layout::Ladder];

fn px(d: Dir, l: Layout, k: usize) -> u64 {
    match l {
        Layout::OneLevel => d.lvl(0),
        Layout::Ladder => d.lvl(k),
    }
}

/// Rests one single-lot maker order per entry of `owners`, maker `k` at `px(d, l, k)`.
fn seed(ctx: &mut TestCtx, d: Dir, l: Layout, owners: &[Address]) -> Vec<[u8; 32]> {
    owners
        .iter()
        .enumerate()
        .map(|(k, &u)| rest(ctx, u, d.maker(), px(d, l, k), 1))
        .collect()
}

/// The maker side as `(price, FIFO, count)` in matching priority.
fn maker_book(ctx: &mut TestCtx, d: Dir) -> Vec<(u64, Vec<[u8; 32]>, u64)> {
    prices(ctx, d.maker())
        .into_iter()
        .map(|p| {
            let (q, c) = level(ctx, d.maker(), p);
            (p, q, c)
        })
        .collect()
}

/// The maker side expected when exactly makers `ids[from..]` survive, untouched, of a book seeded
/// by [`seed`] (maker `k` at `px(d, l, k)`).
fn survivors(d: Dir, l: Layout, ids: &[[u8; 32]], from: usize) -> Vec<(u64, Vec<[u8; 32]>, u64)> {
    match l {
        Layout::OneLevel if from == ids.len() => vec![],
        Layout::OneLevel => {
            vec![(d.lvl(0), ids[from..].to_vec(), (ids.len() - from) as u64)]
        }
        Layout::Ladder => (from..ids.len()).map(|k| (d.lvl(k), vec![ids[k]], 1)).collect(),
    }
}

// ── Shell helpers ────────────────────────────────────────────────────────────────────────────

fn take_logs(ctx: &mut TestCtx) -> Vec<primitives::Log> {
    JournalTr::take_logs(ctx.journal_mut())
}

fn revert_reason(out: &PerpOutput) -> String {
    assert!(out.reverted, "the call did not revert");
    let b = &out.bytes;
    assert_eq!(&b[..4], &[0x08, 0xc3, 0x79, 0xa0], "Error(string) selector");
    let len = U256::from_be_slice(&b[36..68]).to::<usize>();
    String::from_utf8(b[68..68 + len].to_vec()).unwrap()
}

fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> (PerpOutput, Vec<primitives::Log>) {
    let _ = take_logs(ctx);
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(!out.reverted, "unexpected revert: {}", revert_reason(&out));
    let logs = take_logs(ctx);
    (out, logs)
}

/// A REJECTED shell call: exact reason, NO write, no log, the whole world bit-identical.
fn rejected(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) {
    rejected_burning(ctx, caller, input, expect, 0);
}

/// [`rejected`], except that exactly `burn_writes` perp writes (a signed path's signature burn)
/// are expected. The world must still be bit-identical.
fn rejected_burning(
    ctx: &mut TestCtx,
    caller: Address,
    input: &[u8],
    expect: &str,
    burn_writes: u64,
) {
    let _ = take_logs(ctx);
    let before = world(ctx);
    let writes = ctx.perp_write_count();
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(out.reverted, "expected the revert `{expect}`, but the call SUCCEEDED");
    assert_eq!(revert_reason(&out), expect);
    assert_eq!(
        ctx.perp_write_count() - writes,
        burn_writes,
        "[{expect}] the rejected call wrote perp state"
    );
    assert!(take_logs(ctx).is_empty(), "[{expect}] the rejected call logged");
    assert!(world(ctx) == before, "[{expect}] the rejected call changed stored state");
}

fn place(side: u8, price: u64, qty: u64, order_type: u8, tif: u8, flags: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

fn limit(side: u8, price: u64, qty: u64, tif: u8, flags: u8) -> Vec<u8> {
    place(side, price, qty, LIMIT, tif, flags)
}

fn market(side: u8, qty: u64) -> Vec<u8> {
    place(side, 0, qty, MKT, GTC, 0)
}

fn order_id_of(out: &PerpOutput) -> [u8; 32] {
    placeOrderCall::abi_decode_returns(&out.bytes).unwrap().0
}

fn rest(ctx: &mut TestCtx, user: Address, side: u8, price: u64, qty: u64) -> [u8; 32] {
    let (out, logs) = ok(ctx, user, &limit(side, price, qty, GTC, 0));
    assert_eq!(evs::<OrderRested>(&logs).len(), 1, "{user} {side}@{price} did not rest");
    assert!(evs::<Trade>(&logs).is_empty(), "{user} {side}@{price} traded while resting");
    order_id_of(&out)
}

fn evs<E: SolEvent>(logs: &[primitives::Log]) -> Vec<E> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect()
}

fn cancels(logs: &[primitives::Log]) -> Vec<(Address, [u8; 32], u8)> {
    evs::<OrderCancelled>(logs)
        .into_iter()
        .map(|e| (e.user, e.orderId.0, e.reason))
        .collect()
}

/// `Trade` as `(maker order id, quantity)`.
fn fills(logs: &[primitives::Log]) -> Vec<([u8; 32], u64)> {
    evs::<Trade>(logs)
        .into_iter()
        .map(|e| (e.makerOrderId.0, e.quantity))
        .collect()
}

/// Every maker in `ids` filled for 1, in order.
fn one_each(ids: &[[u8; 32]]) -> Vec<([u8; 32], u64)> {
    ids.iter().map(|&id| (id, 1)).collect()
}

/// The single `OrderExpired` as `(orderId, filled, expired, reason)`.
fn expiry(logs: &[primitives::Log]) -> ([u8; 32], u64, u64, u8) {
    let e = evs::<OrderExpired>(logs);
    assert_eq!(e.len(), 1, "exactly one OrderExpired");
    (e[0].orderId.0, e[0].filledQuantity, e[0].expiredQuantity, e[0].reason)
}

fn level(ctx: &mut TestCtx, side: u8, price: u64) -> (Vec<[u8; 32]>, u64) {
    if side == BUY {
        (
            storage::load_bid_level(ctx, MARKET_ID, price).unwrap(),
            storage::load_bid_count(ctx, MARKET_ID, price).unwrap(),
        )
    } else {
        (
            storage::load_ask_level(ctx, MARKET_ID, price).unwrap(),
            storage::load_ask_count(ctx, MARKET_ID, price).unwrap(),
        )
    }
}

fn prices(ctx: &mut TestCtx, side: u8) -> Vec<u64> {
    if side == BUY {
        let mut p = storage::load_bid_prices(ctx, MARKET_ID).unwrap();
        p.sort_unstable_by(|a, b| b.cmp(a));
        p
    } else {
        let mut p = storage::load_ask_prices(ctx, MARKET_ID).unwrap();
        p.sort_unstable();
        p
    }
}

fn amount(ctx: &mut TestCtx, user: Address) -> i64 {
    storage::load_position(ctx, user, MARKET_ID).unwrap().amount
}

/// `user`'s resting order ids on `side`, SORTED (the entry list is price-ordered, not FIFO).
fn open_ids(ctx: &mut TestCtx, user: Address, side: u8) -> Vec<[u8; 32]> {
    let l = if side == BUY {
        storage::load_buy_orders(ctx, user, MARKET_ID).unwrap()
    } else {
        storage::load_sell_orders(ctx, user, MARKET_ID).unwrap()
    };
    let mut ids: Vec<_> = l.iter().map(|e| e.order_id).collect();
    ids.sort();
    ids
}

fn sorted(ids: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let mut v = ids.to_vec();
    v.sort();
    v
}

// ── The world snapshot (encoded blobs, so "identical" is bit-identical) ───────────────────────

#[derive(Debug, PartialEq)]
struct World {
    owned: Vec<(Address, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)>,
    bids: Vec<(u64, Vec<[u8; 32]>, u64)>,
    asks: Vec<(u64, Vec<[u8; 32]>, u64)>,
    orders: Vec<([u8; 32], Option<Vec<u8>>)>,
    hot: Vec<u8>,
    nonces: Vec<u64>,
}

fn enc<T: serde::Serialize>(v: &T) -> Vec<u8> {
    perp_core::codec::encode(v).unwrap()
}

fn world(ctx: &mut TestCtx) -> World {
    let users = [ALICE, BOB, CAROL, DAVE, ADMIN];
    let mut ids: Vec<[u8; 32]> = Vec::new();
    let mut owned = Vec::new();
    let mut nonces = Vec::new();
    for user in users {
        let buys = storage::load_buy_orders(ctx, user, MARKET_ID).unwrap();
        let sells = storage::load_sell_orders(ctx, user, MARKET_ID).unwrap();
        ids.extend(buys.iter().map(|e| e.order_id));
        ids.extend(sells.iter().map(|e| e.order_id));
        owned.push((
            user,
            enc(&storage::load_account(ctx, user).unwrap()),
            enc(&storage::load_position(ctx, user, MARKET_ID).unwrap()),
            enc(&buys),
            enc(&sells),
        ));
        nonces.push(storage::load_user_nonce(ctx, user).unwrap());
    }
    let mut side_levels = |side: u8, ids: &mut Vec<[u8; 32]>| -> Vec<(u64, Vec<[u8; 32]>, u64)> {
        prices(ctx, side)
            .into_iter()
            .map(|p| {
                let (q, c) = level(ctx, side, p);
                ids.extend(q.iter().copied());
                (p, q, c)
            })
            .collect()
    };
    let bids = side_levels(BUY, &mut ids);
    let asks = side_levels(SELL, &mut ids);
    ids.sort();
    ids.dedup();
    let orders = ids
        .into_iter()
        .map(|id| (id, storage::load_order(ctx, &id).unwrap().map(|o| enc(&o))))
        .collect();
    World {
        owned,
        bids,
        asks,
        orders,
        hot: enc(&storage::load_market_hot(ctx, MARKET_ID).unwrap()),
        nonces,
    }
}

// ── Direct placement (for the touch count the shell does not expose) ─────────────────────────

/// `place_order_core` called directly under `budget`, with the caller's next nonce id (committed on
/// success like `placeOrder` does). Returns the outcome or the reject message.
#[allow(clippy::too_many_arguments)]
fn direct(
    ctx: &mut TestCtx,
    user: Address,
    side: u8,
    price: u64,
    qty: u64,
    order_type: u8,
    tif: u8,
    flags: u8,
    budget: crate::trading::TouchBudget,
) -> Result<crate::trading::PlaceOutcome, String> {
    let (id, bumped) = crate::trading::peek_order_id(ctx, user).unwrap();
    let r = crate::trading::place_order_core(
        user,
        id,
        MARKET_ID,
        side,
        price,
        qty,
        order_type,
        tif,
        [0u8; 16],
        flags,
        crate::trading::PlacementOrigin::Submitted,
        budget,
        ctx,
    );
    let _ = take_logs(ctx);
    match r {
        Ok(p) => {
            crate::trading::commit_order_nonce(ctx, user, bumped).unwrap();
            Ok(p)
        }
        Err(e) => Err(e.to_string()),
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// The constants
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// The reject strings state the numbers; they must not drift from the constants.
#[test]
fn reject_strings_state_their_constants() {
    assert_eq!(MAX_TAKER_MAKER_TOUCHES as usize, CAP);
    assert!(TOUCH_CAP_REJECT.starts_with("placeOrder: "));
    assert!(TOUCH_CAP_REJECT.contains(&format!(" {MAX_TAKER_MAKER_TOUCHES} ")));
    let batch = crate::batch::BATCH_TOUCH_CAP_REJECT;
    assert!(batch.starts_with("placeOrder: "));
    assert!(batch.contains(&format!(" {} ", crate::batch::MAX_BATCH_MAKER_TOUCHES)));
    // Neither is in the batch reason table: a batch item reports it as `Other` (255).
    for msg in [TOUCH_CAP_REJECT, batch] {
        assert_eq!(
            crate::batch::reason_code(&crate::errors::perp_err(msg)) as u8,
            R_OTHER,
            "{msg}"
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// T1 GTC
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// T1a-T1d: a submitted GTC against 65 (or 64) single-lot makers.
#[test]
fn t1_gtc_fills_up_to_64_makers_and_rejects_the_65th() {
    for d in DIRS {
        for l in LAYOUTS {
            let tag = format!("{d:?} {l:?}");

            // T1a: qty 63 → 63 fills, 2 left. Touches 63 (read through the direct call).
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, l, &[BOB; 65]);
            let p = direct(&mut ctx, CAROL, d.taker(), d.through(), 63, LIMIT, GTC, 0,
                crate::trading::TouchBudget::SINGLE).unwrap();
            assert_eq!((p.filled, p.touches, p.touch_capped), (63, 63, false), "{tag}");
            assert_eq!(maker_book(&mut ctx, d), survivors(d, l, &ids, 63), "{tag}");

            // T1a again through the shell: the logs.
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, l, &[BOB; 65]);
            let (_, logs) = ok(&mut ctx, CAROL, &limit(d.taker(), d.through(), 63, GTC, 0));
            assert_eq!(fills(&logs), one_each(&ids[..63]), "{tag}");
            assert!(evs::<OrderExpired>(&logs).is_empty() && evs::<OrderRested>(&logs).is_empty());
            assert_eq!(maker_book(&mut ctx, d), survivors(d, l, &ids, 63), "{tag}");
            assert_eq!(amount(&mut ctx, CAROL), 63 * d.sign(), "{tag}");

            // T1b: qty 64 → exactly the cap, never capped; 1 left.
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, l, &[BOB; 65]);
            let (_, logs) = ok(&mut ctx, CAROL, &limit(d.taker(), d.through(), 64, GTC, 0));
            assert_eq!(fills(&logs), one_each(&ids[..64]), "{tag}");
            assert!(evs::<OrderExpired>(&logs).is_empty() && evs::<OrderRested>(&logs).is_empty());
            assert_eq!(maker_book(&mut ctx, d), survivors(d, l, &ids, 64), "{tag}");

            // T1c: qty 65 → the 65th live maker: rejected, nothing written, all 65 intact.
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, l, &[BOB; 65]);
            rejected(&mut ctx, CAROL, &limit(d.taker(), d.through(), 65, GTC, 0), TOUCH_CAP_REJECT);
            assert_eq!(maker_book(&mut ctx, d), survivors(d, l, &ids, 0), "{tag}");

            // T1d: qty 65 against only 64 → the book ran out (not the cap): 64 fills, 1 rests.
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, l, &[BOB; 64]);
            let (out, logs) = ok(&mut ctx, CAROL, &limit(d.taker(), d.through(), 65, GTC, 0));
            assert_eq!(fills(&logs), one_each(&ids), "{tag}");
            let rested = evs::<OrderRested>(&logs);
            assert_eq!(rested.len(), 1, "{tag}");
            assert!(evs::<OrderExpired>(&logs).is_empty());
            assert!(maker_book(&mut ctx, d).is_empty(), "{tag}");
            assert_eq!(level(&mut ctx, d.taker(), d.through()), (vec![order_id_of(&out)], 1));
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// T2 FOK
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// T2a-T2c: a FOK of 64 against 65 makers fills; of 65 against 65 is refused by the PRE-CHECK with
/// the cap message; of 65 against 64 is the ordinary "cannot be fully filled". Both rejects are
/// write-clean.
#[test]
fn t2_fok_rejects_with_the_cap_message_only_when_the_book_could_fill_it() {
    for d in DIRS {
        for l in LAYOUTS {
            let tag = format!("{d:?} {l:?}");
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, l, &[BOB; 65]);
            let (_, logs) = ok(&mut ctx, CAROL, &limit(d.taker(), d.through(), 64, FOK, 0));
            assert_eq!(fills(&logs), one_each(&ids[..64]), "{tag}");
            assert_eq!(maker_book(&mut ctx, d), survivors(d, l, &ids, 64), "{tag}");

            let mut ctx = fresh();
            seed(&mut ctx, d, l, &[BOB; 65]);
            rejected(&mut ctx, CAROL, &limit(d.taker(), d.through(), 65, FOK, 0), TOUCH_CAP_REJECT);

            let mut ctx = fresh();
            seed(&mut ctx, d, l, &[BOB; 64]);
            rejected(&mut ctx, CAROL, &limit(d.taker(), d.through(), 65, FOK, 0), E_FOK);
        }
    }
}

/// T2d (spec §2.3, last bullet): the pre-check passes but the WALK trips. A frozen maker at the
/// head counts in both (it is live), so a FOK of 64 sees 64 live makers with 64 available and
/// passes the pre-check; the walk then cancels the frozen maker (touch 1), fills 63 (touches
/// 64) and needs a 65th live maker for its last unit — the walk's own Reject, also write-clean.
#[test]
fn t2d_a_fok_the_walk_caps_after_a_skipped_maker_is_rejected_write_clean() {
    for d in DIRS {
        let mut ctx = fresh();
        rest(&mut ctx, DAVE, d.maker(), d.lvl(0), 1);
        freeze(&mut ctx, DAVE);
        let ids = seed(&mut ctx, d, Layout::Ladder, &[BOB; 65]);
        rejected(&mut ctx, CAROL, &limit(d.taker(), d.through(), 64, FOK, 0), TOUCH_CAP_REJECT);
        assert_eq!(maker_book(&mut ctx, d).len(), 65, "{d:?}");
        assert_eq!(open_ids(&mut ctx, BOB, d.maker()), sorted(&ids));
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// T3/T4 IOC and market: clamp
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// T3 / T4: an IOC or market order of 100 against 65 makers fills 64 and EXPIRES 36 with reason 0;
/// maker #65 is still at its FIFO head, alone on its level. T3b: an IOC of 64 against 64 fills
/// completely, with no `OrderExpired`.
#[test]
fn t3_t4_ioc_and_market_fill_64_then_expire_the_rest_as_tif() {
    for d in DIRS {
        for l in LAYOUTS {
            for is_market in [false, true] {
                let tag = format!("{d:?} {l:?} market={is_market}");
                let mut ctx = fresh();
                let ids = seed(&mut ctx, d, l, &[BOB; 65]);
                let input = if is_market {
                    market(d.taker(), 100)
                } else {
                    limit(d.taker(), d.through(), 100, IOC, 0)
                };
                let (out, logs) = ok(&mut ctx, CAROL, &input);
                assert_eq!(fills(&logs), one_each(&ids[..64]), "{tag}");
                assert_eq!(expiry(&logs), (order_id_of(&out), 64, 36, X_TIF), "{tag}");
                assert_eq!(maker_book(&mut ctx, d), survivors(d, l, &ids, 64), "{tag}");
                assert_eq!(
                    level(&mut ctx, d.maker(), px(d, l, 64)),
                    (vec![ids[64]], 1),
                    "{tag}: maker #65 heads its level"
                );
                assert_eq!(amount(&mut ctx, CAROL), 64 * d.sign(), "{tag}");
                assert!(storage::load_order(&mut ctx, &order_id_of(&out)).unwrap().is_none());
            }

            // T3b
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, l, &[BOB; 64]);
            let (_, logs) = ok(&mut ctx, CAROL, &limit(d.taker(), d.through(), 64, IOC, 0));
            assert_eq!(fills(&logs), one_each(&ids), "{d:?} {l:?}");
            assert!(evs::<OrderExpired>(&logs).is_empty());
            assert!(maker_book(&mut ctx, d).is_empty());
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// T5/T6 skip-type touches: STP, frozen, K9
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// T5a/T5b: 64 own EXPIRE_MAKER asks, then one third-party ask. An IOC of 1 cancels all 64 own
/// orders (reason 9, FIFO) and the cap trips at the third party: it expires 1 with reason 0 and the
/// third-party order is untouched. A GTC of 1 is refused, all 64 own orders still resting.
#[test]
fn t5ab_stp_cancels_count_as_touches() {
    for d in DIRS {
        for l in LAYOUTS {
            let tag = format!("{d:?} {l:?}");
            let mut owners = vec![ALICE; 64];
            owners.push(BOB);
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, l, &owners);
            rejected(&mut ctx, ALICE, &limit(d.taker(), d.through(), 1, GTC, EM), TOUCH_CAP_REJECT);
            assert_eq!(open_ids(&mut ctx, ALICE, d.maker()), sorted(&ids[..64]), "{tag}");

            let (out, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.through(), 1, IOC, EM));
            let want: Vec<_> = ids[..64].iter().map(|&id| (ALICE, id, C_STP)).collect();
            assert_eq!(cancels(&logs), want, "{tag}");
            assert!(fills(&logs).is_empty(), "{tag}");
            assert_eq!(expiry(&logs), (order_id_of(&out), 0, 1, X_TIF), "{tag}");
            assert_eq!(maker_book(&mut ctx, d), survivors(d, l, &ids, 64), "{tag}");
            assert!(open_ids(&mut ctx, ALICE, d.maker()).is_empty());
        }
    }
}

/// T5c: 63 own orders and one third party — 63 cancels plus 1 fill is exactly 64 touches, so a GTC
/// of 1 is NOT refused.
#[test]
fn t5c_63_stp_cancels_and_one_fill_is_exactly_the_cap() {
    for d in DIRS {
        for l in LAYOUTS {
            let tag = format!("{d:?} {l:?}");
            let mut owners = vec![ALICE; 63];
            owners.push(BOB);
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, l, &owners);
            let p = direct(&mut ctx, ALICE, d.taker(), d.through(), 1, LIMIT, GTC, EM,
                crate::trading::TouchBudget::SINGLE).unwrap();
            assert_eq!((p.filled, p.touches, p.touch_capped), (1, 64, false), "{tag}");

            let mut ctx = fresh();
            let ids2 = seed(&mut ctx, d, l, &owners);
            assert_eq!(ids, ids2);
            let (_, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.through(), 1, GTC, EM));
            let want: Vec<_> = ids[..63].iter().map(|&id| (ALICE, id, C_STP)).collect();
            assert_eq!(cancels(&logs), want, "{tag}");
            assert_eq!(fills(&logs), vec![(ids[63], 1)], "{tag}");
            assert!(maker_book(&mut ctx, d).is_empty(), "{tag}");
        }
    }
}

/// T5d/T5e: 64 makers that the maker guard cancels — frozen by a cross liquidation (reason 3) or
/// failing K9 (reason 2) — then one normal maker. An IOC of 1 cancels the 64 and the cap trips at
/// the normal maker: expire 1, reason 0, the normal maker untouched.
#[test]
fn t5de_frozen_and_k9_rejected_makers_count_as_touches() {
    for d in DIRS {
        for l in LAYOUTS {
            for (frozen, reason) in [(true, C_LIQUIDATION), (false, C_MAKER_INSOLVENT)] {
                let tag = format!("{d:?} {l:?} frozen={frozen}");
                let mut owners = vec![DAVE; 64];
                owners.push(BOB);
                let mut ctx = fresh();
                let ids = seed(&mut ctx, d, l, &owners);
                if frozen {
                    freeze(&mut ctx, DAVE);
                } else {
                    // No wallet: opening the maker's position would be below maintenance (K9).
                    set_wallet(&mut ctx, DAVE, 0);
                }
                let (out, logs) = ok(&mut ctx, CAROL, &limit(d.taker(), d.through(), 1, IOC, 0));
                let want: Vec<_> = ids[..64].iter().map(|&id| (DAVE, id, reason)).collect();
                assert_eq!(cancels(&logs), want, "{tag}");
                assert!(fills(&logs).is_empty(), "{tag}");
                assert_eq!(expiry(&logs), (order_id_of(&out), 0, 1, X_TIF), "{tag}");
                assert_eq!(maker_book(&mut ctx, d), survivors(d, l, &ids, 64), "{tag}");
            }
        }
    }
}

/// T6a: an EXPIRE_TAKER taker whose own order sits at position 65 — the cap trips BEFORE STP looks
/// at it: 64 fills, expire 36 with reason 0 (not 9), the own order still resting.
/// T6b: the own order at position 64 — 63 fills, then STP stops the walk: expire 37, reason 9.
#[test]
fn t6_the_cap_trips_before_stp_sees_an_own_order_at_position_65() {
    for d in DIRS {
        for l in LAYOUTS {
            let tag = format!("{d:?} {l:?}");
            let mut owners = vec![BOB; 64];
            owners.push(ALICE);
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, l, &owners);
            let (out, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.through(), 100, IOC, ET));
            assert_eq!(fills(&logs), one_each(&ids[..64]), "{tag}");
            assert!(cancels(&logs).is_empty(), "{tag}");
            assert_eq!(expiry(&logs), (order_id_of(&out), 64, 36, X_TIF), "{tag}");
            assert_eq!(maker_book(&mut ctx, d), survivors(d, l, &ids, 64), "{tag}");

            let mut owners = vec![BOB; 63];
            owners.push(ALICE);
            owners.push(BOB);
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, l, &owners);
            let (out, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.through(), 100, IOC, ET));
            assert_eq!(fills(&logs), one_each(&ids[..63]), "{tag}");
            assert!(cancels(&logs).is_empty(), "{tag}");
            assert_eq!(expiry(&logs), (order_id_of(&out), 63, 37, X_STP), "{tag}");
            assert_eq!(maker_book(&mut ctx, d), survivors(d, l, &ids, 63), "{tag}");
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// T7 stale ids are free
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// T7: 100 asks on one level, the first 50 cancelled — their ids stay in the lazy FIFO (count 50,
/// 100 ids). A GTC of 50 loads 100 ids but touches only the 50 live ones, so it is not refused (it
/// would be, at 100 > 64, if stale ids counted): full fill, touches 50, and the 50 stale ids are
/// swept with the level.
#[test]
fn t7_stale_ids_do_not_count() {
    for d in DIRS {
        for shell in [false, true] {
            let mut ctx = fresh();
            let ids = seed(&mut ctx, d, Layout::OneLevel, &[BOB; 100]);
            for id in &ids[..50] {
                ok(&mut ctx, BOB, &cancelOrderCall { orderId: (*id).into(), marketId: MARKET_ID }
                    .abi_encode());
            }
            assert_eq!(level(&mut ctx, d.maker(), d.lvl(0)), (ids.clone(), 50), "lazy FIFO");
            if shell {
                let (_, logs) = ok(&mut ctx, CAROL, &limit(d.taker(), d.through(), 50, GTC, 0));
                assert_eq!(fills(&logs), one_each(&ids[50..]), "{d:?}");
            } else {
                let p = direct(&mut ctx, CAROL, d.taker(), d.through(), 50, LIMIT, GTC, 0,
                    crate::trading::TouchBudget::SINGLE).unwrap();
                assert_eq!((p.filled, p.touches, p.touch_capped), (50, 50, false), "{d:?}");
            }
            assert!(maker_book(&mut ctx, d).is_empty(), "{d:?}");
            assert_eq!(level(&mut ctx, d.maker(), d.lvl(0)), (vec![], 0), "stale ids swept");
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// T17 signed
// ═════════════════════════════════════════════════════════════════════════════════════════════

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn register_alice_key(ctx: &mut TestCtx) {
    ok(
        ctx,
        ALICE,
        &registerApiKeyCall {
            keyId: 1,
            pubkey: key().verifying_key().to_bytes().into(),
            expiry: 0,
        }
        .abi_encode(),
    );
}

/// `placeOrderSigned` for ALICE (105-byte message, rebuilt from the ABI doc).
fn signed_place(ctx: &mut TestCtx, side: u8, price: u64, qty: u64, tif: u8, flags: u8) -> Vec<u8> {
    let client = [0u8; 16];
    let mut msg = Vec::new();
    msg.extend_from_slice(b"perpdex_v1_order");
    msg.extend_from_slice(&ctx.chain_id().to_be_bytes());
    msg.extend_from_slice(ALICE.as_slice());
    msg.extend_from_slice(&MARKET_ID.to_be_bytes());
    msg.push(side);
    msg.extend_from_slice(&price.to_be_bytes());
    msg.extend_from_slice(&qty.to_be_bytes());
    msg.extend_from_slice(&[LIMIT, tif]);
    msg.extend_from_slice(&client);
    msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
    msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
    msg.extend_from_slice(&[1, flags]);
    assert_eq!(msg.len(), 105);
    let sig = key().sign(&msg).to_bytes();
    placeOrderSignedCall {
        account: ALICE,
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: LIMIT,
        tif,
        clientOrderId: client.into(),
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 1,
        flags,
        signature: sig.to_vec().into(),
    }
    .abi_encode()
}

/// T17: a signed GTC that trips the cap keeps exactly the signature-burn writes (measured on a twin
/// whose placement fails validation instead) and nothing else: book, positions and orders are
/// bit-identical, nothing is logged, and the signature is spent.
#[test]
fn t17_a_signed_gtc_over_the_cap_retains_only_the_signature_burn() {
    for d in DIRS {
        let burn = {
            let mut twin = fresh();
            register_alice_key(&mut twin);
            let input = signed_place(&mut twin, d.taker(), d.through(), 65, GTC, 0x10);
            let w = twin.perp_write_count();
            let out = run_perp_dex_call(&input, GAS, DAVE, U256::ZERO, false, &mut twin).unwrap();
            assert_eq!(revert_reason(&out), "placeOrder: unknown order flags 0x10");
            twin.perp_write_count() - w
        };
        assert!(burn > 0);
        let mut ctx = fresh();
        register_alice_key(&mut ctx);
        seed(&mut ctx, d, Layout::Ladder, &[BOB; 65]);
        let input = signed_place(&mut ctx, d.taker(), d.through(), 65, GTC, 0);
        rejected_burning(&mut ctx, DAVE, &input, TOUCH_CAP_REJECT, burn);
        rejected_burning(&mut ctx, DAVE, &input, E_DUP_SIG, 0);
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// T8 batch: per-item cap + a 256-touch total
// ═════════════════════════════════════════════════════════════════════════════════════════════

fn item(side: u8, price: u64, qty: u64, order_type: u8, tif: u8) -> PlaceItem {
    PlaceItem {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
}

fn batch(items: Vec<PlaceItem>) -> Vec<u8> {
    batchPlaceOrdersCall { orders: items }.abi_encode()
}

/// Batch status records `(tag, orderId, reason)`.
fn records(out: &PerpOutput) -> Vec<(u8, [u8; 32], u8)> {
    let blob = batchPlaceOrdersCall::abi_decode_returns(&out.bytes).unwrap();
    assert_eq!(blob.len() % 34, 0);
    blob.chunks(34)
        .map(|r| (r[0], r[1..33].try_into().unwrap(), r[33]))
        .collect()
}

/// `(tag, reason)` of each record.
fn tags(out: &PerpOutput) -> Vec<(u8, u8)> {
    records(out).into_iter().map(|(t, _, r)| (t, r)).collect()
}

/// 320 single-lot makers on one level: BOB's 160, then CAROL's 160 (each under the 200 cap).
fn deep_book(ctx: &mut TestCtx, d: Dir) -> Vec<[u8; 32]> {
    let mut owners = vec![BOB; 160];
    owners.extend([CAROL; 160]);
    seed(ctx, d, Layout::OneLevel, &owners)
}

fn ioc(d: Dir, qty: u64) -> PlaceItem {
    item(d.taker(), d.through(), qty, LIMIT, IOC)
}

fn gtc(d: Dir, qty: u64) -> PlaceItem {
    item(d.taker(), d.through(), qty, LIMIT, GTC)
}

/// T8a: a GTC item needing 65 makers is `Rejected` (reason 255) and the batch carries on: the next
/// item, a GTC that crosses nothing, rests (`Accepted`). The 65 makers are intact.
#[test]
fn t8a_a_gtc_item_over_the_cap_is_rejected_and_the_batch_continues() {
    for d in DIRS {
        let mut ctx = fresh();
        let ids = seed(&mut ctx, d, Layout::Ladder, &[BOB; 65]);
        let passive = if d.0 == BUY { MARK - 500 } else { MARK + 500 };
        let (out, logs) = ok(
            &mut ctx,
            ALICE,
            &batch(vec![gtc(d, 65), item(d.taker(), passive, 1, LIMIT, GTC)]),
        );
        assert_eq!(tags(&out), vec![(TAG_REJECTED, R_OTHER), (TAG_ACCEPTED, R_NONE)], "{d:?}");
        assert!(fills(&logs).is_empty());
        assert_eq!(maker_book(&mut ctx, d), survivors(d, Layout::Ladder, &ids, 0));
        assert_eq!(level(&mut ctx, d.taker(), passive), (vec![records(&out)[1].1], 1));
    }
}

/// T8b: five IOC items of 64 against 320 makers. Items 1-4 take 64 each — 256, the batch total —
/// and item 5 has nothing left: it fills 0 and expires all 64 (reason 0). All five are accepted
/// (terminal). 64 makers survive.
#[test]
fn t8b_the_batch_total_runs_out_after_256_touches() {
    for d in DIRS {
        let mut ctx = fresh();
        let ids = deep_book(&mut ctx, d);
        let (out, logs) = ok(&mut ctx, ALICE, &batch(vec![ioc(d, 64); 5]));
        assert_eq!(tags(&out), vec![(TAG_TERMINAL, R_NONE); 5], "{d:?}");
        assert_eq!(fills(&logs), one_each(&ids[..256]), "{d:?}");
        assert_eq!(expiry(&logs), (records(&out)[4].1, 0, 64, X_TIF), "{d:?}");
        assert_eq!(maker_book(&mut ctx, d), survivors(d, Layout::OneLevel, &ids, 256));
        assert_eq!(amount(&mut ctx, ALICE), 256 * d.sign());
    }
}

/// T8c/T8c': IOC items of 64, 64, 64 and 48 spend 240 of the 256, leaving 16. A GTC item of 20
/// would need 20 > 16: `Rejected` (reason 255) with the BATCH's message (read off a direct call
/// under the same 16-touch budget). An IOC item of 20 instead fills 16 and expires 4.
#[test]
fn t8c_an_item_gets_only_what_the_batch_has_left() {
    for d in DIRS {
        for last_is_gtc in [true, false] {
            let tag = format!("{d:?} gtc={last_is_gtc}");
            let mut ctx = fresh();
            let ids = deep_book(&mut ctx, d);
            let last = if last_is_gtc { gtc(d, 20) } else { ioc(d, 20) };
            let items = vec![ioc(d, 64), ioc(d, 64), ioc(d, 64), ioc(d, 48), last];
            let (out, logs) = ok(&mut ctx, ALICE, &batch(items));
            let mut want = vec![(TAG_TERMINAL, R_NONE); 4];
            if last_is_gtc {
                want.push((TAG_REJECTED, R_OTHER));
                assert_eq!(tags(&out), want, "{tag}");
                assert_eq!(fills(&logs), one_each(&ids[..240]), "{tag}");
                assert!(evs::<OrderExpired>(&logs).is_empty(), "{tag}");
                assert_eq!(maker_book(&mut ctx, d), survivors(d, Layout::OneLevel, &ids, 240));
                // The message, from the same budget the item got.
                let budget = crate::trading::TouchBudget {
                    limit: 16,
                    reject_msg: crate::batch::BATCH_TOUCH_CAP_REJECT,
                };
                let r = direct(&mut ctx, ALICE, d.taker(), d.through(), 20, LIMIT, GTC, 0, budget);
                assert_eq!(r.unwrap_err(), crate::batch::BATCH_TOUCH_CAP_REJECT, "{tag}");
            } else {
                want.push((TAG_TERMINAL, R_NONE));
                assert_eq!(tags(&out), want, "{tag}");
                assert_eq!(fills(&logs), one_each(&ids[..256]), "{tag}");
                assert_eq!(expiry(&logs), (records(&out)[4].1, 16, 4, X_TIF), "{tag}");
                assert_eq!(maker_book(&mut ctx, d), survivors(d, Layout::OneLevel, &ids, 256));
            }
        }
    }
}

/// T8d: a rejected item spends nothing. A GTC item needing 65 makers is `Rejected`; the four IOC
/// items of 64 after it still get 64 each (256 in total).
#[test]
fn t8d_a_rejected_item_spends_none_of_the_batch_budget() {
    for d in DIRS {
        let mut ctx = fresh();
        let ids = deep_book(&mut ctx, d);
        let (out, logs) = ok(
            &mut ctx,
            ALICE,
            &batch(vec![gtc(d, 65), ioc(d, 64), ioc(d, 64), ioc(d, 64), ioc(d, 64)]),
        );
        let mut want = vec![(TAG_REJECTED, R_OTHER)];
        want.extend([(TAG_TERMINAL, R_NONE); 4]);
        assert_eq!(tags(&out), want, "{d:?}");
        assert_eq!(fills(&logs), one_each(&ids[..256]), "{d:?}");
        assert!(evs::<OrderExpired>(&logs).is_empty(), "{d:?}");
        assert_eq!(maker_book(&mut ctx, d), survivors(d, Layout::OneLevel, &ids, 256));
    }
}

/// `batchPlaceOrdersSigned` for ALICE (message rebuilt from the ABI doc).
fn signed_batch(ctx: &mut TestCtx, items: Vec<PlaceItem>) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(b"perpdex_v1_batch_order");
    msg.extend_from_slice(&ctx.chain_id().to_be_bytes());
    msg.extend_from_slice(ALICE.as_slice());
    msg.push(1); // keyId
    msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
    msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
    msg.extend_from_slice(&(items.len() as u32).to_be_bytes());
    for o in &items {
        msg.extend_from_slice(&o.marketId.to_be_bytes());
        msg.push(o.side);
        msg.extend_from_slice(&o.price.to_be_bytes());
        msg.extend_from_slice(&o.quantity.to_be_bytes());
        msg.push(o.orderType);
        msg.push(o.tif);
        msg.extend_from_slice(&o.clientOrderId.0);
        msg.push(o.flags);
    }
    let sig = key().sign(&msg).to_bytes();
    batchPlaceOrdersSignedCall {
        account: ALICE,
        keyId: 1,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        orders: items,
        signature: sig.to_vec().into(),
    }
    .abi_encode()
}

/// T8e: the signed batch keeps the same total — T8b's five IOC items of 64 give identical tags,
/// fills and expiry.
#[test]
fn t8e_the_signed_batch_has_the_same_total() {
    for d in DIRS {
        let mut ctx = fresh();
        register_alice_key(&mut ctx);
        let ids = deep_book(&mut ctx, d);
        let input = signed_batch(&mut ctx, vec![ioc(d, 64); 5]);
        let (out, logs) = ok(&mut ctx, DAVE, &input);
        assert_eq!(tags(&out), vec![(TAG_TERMINAL, R_NONE); 5], "{d:?}");
        assert_eq!(fills(&logs), one_each(&ids[..256]), "{d:?}");
        assert_eq!(expiry(&logs), (records(&out)[4].1, 0, 64, X_TIF), "{d:?}");
        assert_eq!(maker_book(&mut ctx, d), survivors(d, Layout::OneLevel, &ids, 256));
        assert_eq!(amount(&mut ctx, ALICE), 256 * d.sign());
    }
}
