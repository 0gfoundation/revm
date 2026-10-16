//! **Self-trade prevention (STP) and the per-market open-order cap, written from the plan
//! (`STP_PLAN_final.md`: the body, overridden by its Appendix B) by an independent writer — NOT
//! from the implementation's own tests** (`trading::tests::stp_matching`, `risk::tests::algo_stp`,
//! `risk::tests::algo_trigger_rejected`, …).
//!
//! Every call goes through the full shell (`run_perp_dex_call`), so the gas charge, the EOA depth
//! guard, the commit-only #23 tripwire (a `debug_assert` on any write-then-revert), the debug event
//! guards in `crate::events` and the end-of-call `AccountBalanceChanged` drain are all in the loop.
//!
//! Every rejection is asserted four ways (see [`rejected`]): the exact reason string,
//! `perp_write_count` did not move (or, on a signed path, moved only by the signature burn that
//! path performs before the reject — plan §4), the call logged nothing, and the whole observable
//! world — every watched account / position / order list / conditional list, every book level's
//! FIFO and count, every order blob, the BBO + last-traded cache, the fee total and the mid-price
//! window — is BIT-identical (encoded blobs, not field reads).
//!
//! Every test that walks the book runs for BOTH taker sides ([`DIRS`]): the maker levels sit at
//! `MARK ± 100·k` on the side the taker crosses ([`Dir::lvl`]).
//!
//! Self-contained: the market fixture is adapted from a COPY of the risk suite's (not a reach into
//! it), and the direct-path order ids the engine derives are recomputed here as
//! `keccak256(owner ‖ nonce_be8)` straight from the ABI doc ([`oid`]), so no assertion can pass
//! merely because the test and the engine call the same helper.
//!
//! Plan items are cited inline (`T1`…`T25`, `B1`/`B2`, `D9`/`D11`, `§2.3`).

use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use ed25519_dalek::{Signer, SigningKey};
use perp_core::keys::{ask_level_key, bid_level_key};
use primitives::{address, hardfork::SpecId, keccak256, Address, FixedBytes, B256, U256};

use crate::call::PerpOutput;
use crate::host::PerpHost;
use crate::interface::IPerpDex::{
    asSubAccountCall, batchPlaceOrdersCall, batchPlaceOrdersSignedCall, cancelAlgoOrderCall,
    cancelOrderCall, createSubAccountCall, getAlgoOrderCall, getOpenOrdersCall, getOrderCall,
    internalTransferCall, placeAlgoOrderCall, placeAlgoOrderSignedCall, placeOrderCall,
    placeOrderSignedCall, registerApiKeyCall, setUserFeeRatesCall, updateIndexPriceCall,
    AccountBalanceChanged, AlgoOrderCancelled, AlgoOrderPlaced, AlgoOrderTriggered,
    FundingSettled, Liquidation, OrderCancelled, OrderExpired, OrderPlaced, OrderRested,
    PlaceItem, PositionChanged, Trade,
};
use crate::types::{AccountUpdateReason, MarginTiers, Market, Order, PerpPosition, UserAccount};
use crate::{run_perp_dex_call, storage, PERP_DEX_ADDRESS, USDC_ADDRESS};

// ── Market fixture (adapted from `risk::tests`) ────────────────────────────────────────────────

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

/// The owner under test in every scenario: the account whose own orders STP meets.
const ALICE: Address = address!("1111111111111111111111111111111111111111");
/// Third parties.
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
const DAVE: Address = address!("4444444444444444444444444444444444444444");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const MARKET_ID: u64 = 1;
/// $100.00 at price_decimals 2.
const MARK: u64 = 10_000;
/// One unit at $100 in 6-dp quote units: `calc_value(10_000, 1)` at base 0 / price 2.
const UNIT_VALUE: i64 = 100_000_000;
/// $1e9 — large enough that margin never interferes unless a test sets the wallet itself.
const WALLET: i64 = 1_000_000_000_000_000;
const GAS: u64 = 30_000_000;

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, BOB, CAROL, DAVE, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

/// Market 1 at a $100 mark (base 0 / price 2 decimals, tick 1, step 1, min 1, no fees, no funding,
/// the default 10% fill-time band → in band iff 9_000 <= P <= 11_000), and a $1e9 perp wallet for
/// ALICE, BOB, CAROL and DAVE. Incidental logs discarded.
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

/// Plants `perp` as `user`'s perp wallet, keeping every other account field.
fn set_wallet(ctx: &mut TestCtx, user: Address, perp: i64) {
    let mut a = storage::load_account(ctx, user).unwrap();
    a.perp_wallet_balance = perp;
    storage::save_account(ctx, user, a, AccountUpdateReason::Adjustment).unwrap();
}

/// Seeds `user` with `amount` contracts entered at $100 (OFF-BOOK, so `last_traded` stays 0 and an
/// oracle tick sets `mark == index` exactly — the median of `(index, index + basis, index)`).
fn seed_pos(ctx: &mut TestCtx, user: Address, amount: i64, leverage: u64) {
    storage::save_position(
        ctx,
        user,
        MARKET_ID,
        &PerpPosition {
            amount,
            v_quote_balance: -amount * UNIT_VALUE,
            margin: amount.abs() * UNIT_VALUE / leverage as i64,
            leverage,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    let _ = take_logs(ctx);
}

fn set_ts(ctx: &mut TestCtx, ts: u64) {
    ctx.block.timestamp = U256::from(ts);
}

// ── Spec constants ───────────────────────────────────────────────────────────────────────────

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const MKT: u8 = 1;
const GTC: u8 = 0;
const IOC: u8 = 1;
const FOK: u8 = 2;
const PO: u8 = 3;

/// `flags` bit 0.
const RO: u8 = 0b0001;
/// `flags` bits 3:2 (§1.1), an ENUM: 00 EXPIRE_MAKER (default), 01 EXPIRE_TAKER, 10 EXPIRE_BOTH,
/// 11 reserved (rejected).
const EM: u8 = 0b0000;
const ET: u8 = 0b0100;
const EB: u8 = 0b1000;
const RSV: u8 = 0b1100;
const MODES: [u8; 3] = [EM, ET, EB];

/// `CancelReason` (§3.1, D11).
const C_USER: u8 = 0;
const C_STP: u8 = 9;
const C_TRIGGER_REJECTED: u8 = 10;
/// `OrderExpired.reason` (§3.2): 0 TIF / liquidity / band, 9 STP.
const X_TIF: u8 = 0;
const X_STP: u8 = 9;

/// `OrderStatus`.
const S_OPEN: u8 = 0;
const S_PARTIAL: u8 = 1;

/// B2: book buys + book sells + untriggered conditionals, per (user, market).
const CAP: usize = 200;
/// The conditional sub-cap that stays inside it.
const ALGO_CAP: usize = 32;

/// Batch status tags / reasons.
const TAG_REJECTED: u8 = 0;
const TAG_ACCEPTED: u8 = 1;
const TAG_TERMINAL: u8 = 2;
const B_POST_ONLY: u8 = 27;
const B_MAX_OPEN: u8 = 33;
const B_STP_MODE: u8 = 34;

/// AlgoOrderType.
const STOP: u8 = 0;
const STOP_MARKET: u8 = 1;

const ALGO_FLAG_CLOSE_POSITION: u8 = 0b0010;

const SIGNED_TS: u64 = 1; // == the default test block's timestamp
const SIGNED_RECV: u64 = 60;

// ── Exact reject reasons ─────────────────────────────────────────────────────────────────────

const E_CAP: &str = "placeOrder: max open orders reached";
const E_ALGO_CAP: &str = "placeAlgoOrder: max open orders reached";
const E_ALGO_SUBCAP: &str = "placeAlgoOrder: too many open conditional orders in this market";
const E_MODE: &str = "placeOrder: invalid self-trade prevention mode";
const E_ALGO_MODE: &str = "placeAlgoOrder: invalid self-trade prevention mode";
const E_ALGO_MODE_SIGNED: &str =
    "placeAlgoOrderSigned: placeAlgoOrder: invalid self-trade prevention mode";
const E_ALGO_UNKNOWN: &str = "placeAlgoOrder: unknown flag bits";
const E_PO_CROSS: &str = "placeOrder: PostOnly order would match";
const E_MARGIN: &str = "placeOrder: insufficient perp wallet for margin";
const E_NOT_FOUND: &str = "getOrder: order not found";
const E_DUP_SIG: &str = "placeOrderSigned: duplicate signature (already submitted)";
const E_DUP_ALGO_SIG: &str = "placeAlgoOrderSigned: duplicate signature (already submitted)";

fn e_unknown(bits: u8) -> String {
    format!("placeOrder: unknown order flags 0x{bits:02x}")
}

// ── Direction ────────────────────────────────────────────────────────────────────────────────

/// A walk direction, named by the TAKER's side. The maker levels the taker crosses sit at
/// `lvl(0) = MARK` (the touch) and then one 100-tick step deeper per level.
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
    /// Maker level `k` from the touch: asks `10_000, 10_100, …` for a buy taker, bids
    /// `10_000, 9_900, …` for a sell taker. All in band for `k <= 10`.
    fn lvl(self, k: u64) -> u64 {
        if self.0 == BUY {
            MARK + 100 * k
        } else {
            MARK - 100 * k
        }
    }
    /// A quote on the TAKER's own side that crosses nothing the tests place.
    fn passive(self) -> u64 {
        if self.0 == BUY {
            MARK - 500
        } else {
            MARK + 500
        }
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

/// A static view call through the shell.
fn view(ctx: &mut TestCtx, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, GAS, DAVE, U256::ZERO, true, ctx).unwrap()
}

/// A REJECTED shell call: exact reason, NO write, no log, the whole world bit-identical.
fn rejected(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) {
    rejected_burning(ctx, caller, input, expect, 0);
}

/// [`rejected`], except that exactly `burn_writes` perp writes are expected: the signature burn
/// that every signed placement performs BEFORE the placement can reject (plan §4). The world must
/// still be bit-identical — the burn lives in the seen-signature set, which is not part of it.
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

// ── Calldata builders ────────────────────────────────────────────────────────────────────────

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

fn market(side: u8, qty: u64, flags: u8) -> Vec<u8> {
    place(side, 0, qty, MKT, GTC, flags)
}

fn cancel(id: [u8; 32]) -> Vec<u8> {
    cancelOrderCall {
        orderId: id.into(),
        marketId: MARKET_ID,
    }
    .abi_encode()
}

fn item(side: u8, price: u64, qty: u64, order_type: u8, tif: u8, flags: u8) -> PlaceItem {
    PlaceItem {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
        flags,
    }
}

fn batch(items: Vec<PlaceItem>) -> Vec<u8> {
    batchPlaceOrdersCall { orders: items }.abi_encode()
}

/// A conditional order. `order_type` is the AlgoOrderType (STOP / STOP_MARKET).
fn algo(
    side: u8,
    order_type: u8,
    trigger: u64,
    price: u64,
    qty: u64,
    tif: u8,
    flags: u8,
) -> Vec<u8> {
    placeAlgoOrderCall {
        algoType: 0,
        marketId: MARKET_ID,
        side,
        orderType: order_type,
        triggerPrice: trigger,
        price,
        quantity: qty,
        tif,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

fn order_id_of(out: &PerpOutput) -> [u8; 32] {
    placeOrderCall::abi_decode_returns(&out.bytes).unwrap().0
}

/// The direct-path order id, recomputed from the ABI doc: `keccak256(account(20) ‖ nonce(8 BE))`.
fn oid(user: Address, nonce: u64) -> [u8; 32] {
    let mut pre = user.to_vec();
    pre.extend_from_slice(&nonce.to_be_bytes());
    keccak256(&pre).0
}

/// Batch status records `tag(1) ‖ orderId(32) ‖ reason(1)`.
fn batch_records(out: &PerpOutput) -> Vec<(u8, [u8; 32], u8)> {
    let blob = batchPlaceOrdersCall::abi_decode_returns(&out.bytes).unwrap();
    assert_eq!(blob.len() % 34, 0);
    blob.chunks(34)
        .map(|r| (r[0], r[1..33].try_into().unwrap(), r[33]))
        .collect()
}

// ── Placement shortcuts ──────────────────────────────────────────────────────────────────────

/// A resting order that crossed nothing: asserts it emitted `OrderRested` and no `Trade`.
fn rest_with(
    ctx: &mut TestCtx,
    user: Address,
    side: u8,
    price: u64,
    qty: u64,
    tif: u8,
    flags: u8,
) -> [u8; 32] {
    let (out, logs) = ok(ctx, user, &limit(side, price, qty, tif, flags));
    assert_eq!(evs::<OrderRested>(&logs).len(), 1, "{user} {side}@{price} did not rest");
    assert!(evs::<Trade>(&logs).is_empty(), "{user} {side}@{price} traded while resting");
    order_id_of(&out)
}

/// A plain GTC that rests (flags 0).
fn rest(ctx: &mut TestCtx, user: Address, side: u8, price: u64, qty: u64) -> [u8; 32] {
    rest_with(ctx, user, side, price, qty, GTC, 0)
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

/// The oracle tick, through the shell, at `ts = 31` (aligned to 30). It must SUCCEED.
fn tick(ctx: &mut TestCtx, index: u64) -> Vec<primitives::Log> {
    let (_, logs) = ok(
        ctx,
        ADMIN,
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: index,
            timestamp: 31,
        }
        .abi_encode(),
    );
    logs
}

// ── Event helpers ────────────────────────────────────────────────────────────────────────────

/// `E` decoded out of `logs`, in stream order.
fn evs<E: SolEvent>(logs: &[primitives::Log]) -> Vec<E> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect()
}

/// Every log as `(event name, subject)`: the user for the per-user events, the MAKER for a
/// `Trade`, zero otherwise. Unknown events are `"?"` (so an unexpected event fails an exact match).
fn seq(logs: &[primitives::Log]) -> Vec<(&'static str, Address)> {
    logs.iter()
        .map(|log| {
            let t = log.data.topics();
            let d = &log.data.data;
            let h = t.first().copied();
            macro_rules! pick {
                ($($e:ident => $subj:expr),* $(,)?) => {
                    $(if h == Some($e::SIGNATURE_HASH) {
                        let e = $e::decode_raw_log(t, d).unwrap();
                        #[allow(clippy::redundant_closure_call)]
                        return (stringify!($e), ($subj)(&e));
                    })*
                };
            }
            pick!(
                OrderPlaced => |e: &OrderPlaced| e.user,
                OrderRested => |e: &OrderRested| e.user,
                OrderCancelled => |e: &OrderCancelled| e.user,
                OrderExpired => |e: &OrderExpired| e.user,
                Trade => |e: &Trade| e.maker,
                AccountBalanceChanged => |e: &AccountBalanceChanged| e.user,
                PositionChanged => |e: &PositionChanged| e.user,
                FundingSettled => |e: &FundingSettled| e.user,
                AlgoOrderPlaced => |e: &AlgoOrderPlaced| e.user,
                AlgoOrderTriggered => |e: &AlgoOrderTriggered| e.user,
                AlgoOrderCancelled => |e: &AlgoOrderCancelled| e.user,
                Liquidation => |e: &Liquidation| e.user,
            );
            ("?", Address::ZERO)
        })
        .collect()
}

/// The event names alone.
fn names(logs: &[primitives::Log]) -> Vec<&'static str> {
    seq(logs).into_iter().map(|(n, _)| n).collect()
}

/// `OrderCancelled` as `(user, orderId, reason)`.
fn cancels(logs: &[primitives::Log]) -> Vec<(Address, [u8; 32], u8)> {
    evs::<OrderCancelled>(logs)
        .into_iter()
        .map(|e| (e.user, e.orderId.0, e.reason))
        .collect()
}

/// `Trade` as `(taker, maker, price, quantity)`.
fn trades(logs: &[primitives::Log]) -> Vec<(Address, Address, u64, u64)> {
    evs::<Trade>(logs)
        .into_iter()
        .map(|e| (e.taker, e.maker, e.price, e.quantity))
        .collect()
}

/// The single `OrderExpired` as `(orderId, filled, expired, reason)`.
fn expiry(logs: &[primitives::Log]) -> ([u8; 32], u64, u64, u8) {
    let e = evs::<OrderExpired>(logs);
    assert_eq!(e.len(), 1, "exactly one OrderExpired: {:?}", names(logs));
    (e[0].orderId.0, e[0].filledQuantity, e[0].expiredQuantity, e[0].reason)
}

// ── State helpers ────────────────────────────────────────────────────────────────────────────

/// A maker-side level: `(FIFO ids, live count)`.
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

/// One side's price index, put in matching priority here (bids best-first = descending, asks
/// ascending) so the assertions do not depend on the stored order.
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

/// `(best_bid, best_ask)` off the cache.
fn bbo(ctx: &mut TestCtx) -> (u64, u64) {
    let h = storage::load_market_hot(ctx, MARKET_ID).unwrap();
    (h.best_bid, h.best_ask)
}

fn last_traded(ctx: &mut TestCtx) -> u64 {
    storage::load_market_hot(ctx, MARKET_ID).unwrap().last_traded
}

fn fee_total(ctx: &mut TestCtx) -> u64 {
    storage::load_market_fee_total(ctx, MARKET_ID).unwrap()
}

fn order(ctx: &mut TestCtx, id: [u8; 32]) -> Option<Order> {
    storage::load_order(ctx, &id).unwrap()
}

/// `user`'s resting order ids on `side`, as the per-user entry list holds them.
fn open_ids(ctx: &mut TestCtx, user: Address, side: u8) -> Vec<[u8; 32]> {
    let l = if side == BUY {
        storage::load_buy_orders(ctx, user, MARKET_ID).unwrap()
    } else {
        storage::load_sell_orders(ctx, user, MARKET_ID).unwrap()
    };
    l.iter().map(|e| e.order_id).collect()
}

/// B2's count: book buys + book sells + untriggered conditionals.
fn open_count(ctx: &mut TestCtx, user: Address) -> usize {
    open_ids(ctx, user, BUY).len()
        + open_ids(ctx, user, SELL).len()
        + storage::load_user_algo_orders(ctx, user, MARKET_ID).unwrap().len()
}

fn amount(ctx: &mut TestCtx, user: Address) -> i64 {
    storage::load_position(ctx, user, MARKET_ID).unwrap().amount
}

fn nonce(ctx: &mut TestCtx, user: Address) -> u64 {
    storage::load_user_nonce(ctx, user).unwrap()
}

/// `getOrder` through the shell: `Ok((filled, status, flags))` or the revert reason.
fn get_order(ctx: &mut TestCtx, id: [u8; 32]) -> Result<(u64, u8, u8), String> {
    let out = view(
        ctx,
        &getOrderCall {
            orderId: id.into(),
            marketId: MARKET_ID,
        }
        .abi_encode(),
    );
    if out.reverted {
        return Err(revert_reason(&out));
    }
    let r = getOrderCall::abi_decode_returns(&out.bytes).unwrap();
    Ok((r.filled, r.status, r.flags))
}

/// `getOpenOrders(user)` flags, by order id.
fn open_order_flags(ctx: &mut TestCtx, user: Address, id: [u8; 32]) -> u8 {
    let out = view(ctx, &getOpenOrdersCall { user, marketId: MARKET_ID }.abi_encode());
    assert!(!out.reverted);
    let r = getOpenOrdersCall::abi_decode_returns(&out.bytes).unwrap();
    let i = r.orderIds.iter().position(|x| x.0 == id).expect("listed by getOpenOrders");
    r.flags[i]
}

/// The mid-price window's `(last_sample_ts, last_mid_price, count)`.
fn mid_window(ctx: &mut TestCtx) -> (u64, u64, u8) {
    let w = storage::load_price_basis_window(ctx, MARKET_ID).unwrap();
    (w.last_sample_ts, w.last_mid_price, w.count)
}

/// `getAccount(user).availableBalance`-equivalent: `wallet − Σ ooIM` (single market).
fn available(ctx: &mut TestCtx, user: Address) -> i64 {
    let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    let pos = storage::load_position(ctx, user, MARKET_ID).unwrap();
    let oo = crate::margin_view::position_open_order_margin(&market, &pos).unwrap();
    storage::load_account(ctx, user).unwrap().perp_wallet_balance - oo as i64
}

// ── The world snapshot ───────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
struct Owned {
    user: Address,
    account: Vec<u8>,
    position: Vec<u8>,
    buys: Vec<u8>,
    sells: Vec<u8>,
    algos: Vec<[u8; 32]>,
    markets: Vec<u64>,
}

#[derive(Debug, PartialEq)]
struct World {
    owned: Vec<Owned>,
    bids: Vec<(u64, Vec<[u8; 32]>, u64)>,
    asks: Vec<(u64, Vec<[u8; 32]>, u64)>,
    orders: Vec<([u8; 32], Option<Vec<u8>>)>,
    hot: Vec<u8>,
    fee_total: u64,
    window: Vec<u8>,
    insurance_fund: u64,
}

thread_local! {
    /// Extra addresses (sub-accounts) a test wants in the world snapshot.
    static EXTRA_WATCHED: std::cell::RefCell<Vec<Address>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn enc<T: serde::Serialize>(v: &T) -> Vec<u8> {
    perp_core::codec::encode(v).unwrap()
}

fn world(ctx: &mut TestCtx) -> World {
    let mut users = vec![ALICE, BOB, CAROL, DAVE, ADMIN];
    EXTRA_WATCHED.with(|e| users.extend(e.borrow().iter().copied()));
    let mut ids: Vec<[u8; 32]> = Vec::new();
    let mut owned = Vec::new();
    for user in users {
        let buys = storage::load_buy_orders(ctx, user, MARKET_ID).unwrap();
        let sells = storage::load_sell_orders(ctx, user, MARKET_ID).unwrap();
        let algos = storage::load_user_algo_orders(ctx, user, MARKET_ID).unwrap();
        ids.extend(buys.iter().map(|e| e.order_id));
        ids.extend(sells.iter().map(|e| e.order_id));
        ids.extend(algos.iter().copied());
        owned.push(Owned {
            user,
            account: enc(&storage::load_account(ctx, user).unwrap()),
            position: enc(&storage::load_position(ctx, user, MARKET_ID).unwrap()),
            buys: enc(&buys),
            sells: enc(&sells),
            algos,
            markets: storage::load_user_markets(ctx, user).unwrap(),
        });
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
        fee_total: fee_total(ctx),
        window: enc(&storage::load_price_basis_window(ctx, MARKET_ID).unwrap()),
        insurance_fund: storage::load_insurance_fund(ctx).unwrap(),
    }
}

// ── Signing (messages rebuilt from the ABI docs, independently of the engine) ─────────────────

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

/// ALICE registers `key()` in slot 1.
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

/// `placeOrderSigned` for ALICE (105-byte message).
#[allow(clippy::too_many_arguments)]
fn signed_place(
    ctx: &mut TestCtx,
    side: u8,
    price: u64,
    qty: u64,
    tif: u8,
    flags: u8,
    salt: u8,
) -> Vec<u8> {
    let mut client = [0u8; 16];
    client[15] = salt;
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
    msg.extend_from_slice(&[1, flags]); // keyId, flags
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

/// `batchPlaceOrdersSigned` for ALICE.
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
    assert_eq!(msg.len(), 22 + 8 + 20 + 1 + 8 + 8 + 4 + items.len() * 44);
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

/// `placeAlgoOrderSigned` for ALICE (114-byte message): a STOP_MARKET `side` of `qty`.
fn signed_algo(ctx: &mut TestCtx, side: u8, trigger: u64, qty: u64, flags: u8) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(b"perpdex_v1_algo");
    msg.extend_from_slice(&ctx.chain_id().to_be_bytes());
    msg.extend_from_slice(ALICE.as_slice());
    msg.push(0); // algoType
    msg.extend_from_slice(&MARKET_ID.to_be_bytes());
    msg.extend_from_slice(&[side, STOP_MARKET]);
    msg.extend_from_slice(&trigger.to_be_bytes());
    msg.extend_from_slice(&0u64.to_be_bytes()); // price
    msg.extend_from_slice(&qty.to_be_bytes());
    msg.extend_from_slice(&[0, 0]); // tif, workingType
    msg.extend_from_slice(&[0u8; 16]); // clientAlgoId
    msg.push(flags);
    msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
    msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
    msg.push(1); // keyId
    assert_eq!(msg.len(), 114);
    let sig = key().sign(&msg).to_bytes();
    placeAlgoOrderSignedCall {
        account: ALICE,
        algoType: 0,
        marketId: MARKET_ID,
        side,
        orderType: STOP_MARKET,
        triggerPrice: trigger,
        price: 0,
        quantity: qty,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 1,
        signature: sig.to_vec().into(),
    }
    .abi_encode()
}

/// Rests one order of `qty` per `(user, qty)` at maker level `k` of `d`, level by level and in FIFO
/// order; returns the ids per level.
fn book(ctx: &mut TestCtx, d: Dir, levels: &[&[(Address, u64)]]) -> Vec<Vec<[u8; 32]>> {
    levels
        .iter()
        .enumerate()
        .map(|(k, lv)| {
            lv.iter()
                .map(|&(u, q)| rest(ctx, u, d.maker(), d.lvl(k as u64), q))
                .collect()
        })
        .collect()
}

/// `n` resting orders of qty 1 for `user` on `side` at `price`.
fn fill_book(ctx: &mut TestCtx, user: Address, side: u8, price: u64, n: usize) -> Vec<[u8; 32]> {
    (0..n).map(|_| rest(ctx, user, side, price, 1)).collect()
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §1 Encoding: the reserved mode, unknown bits, echoes (T12, T13, T14)
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// T12 / §1.1 / §4: the reserved mode `0b11` is refused by `placeOrder` for EVERY kind — FOK and
/// PostOnly included, where a valid mode is inert — with and without reduce-only, before any
/// write and before any id is consumed.
#[test]
fn t12_place_order_refuses_the_reserved_mode_for_every_kind() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 10, 1); // a reduce-only sell would be admissible past the decode
    seed_pos(&mut ctx, CAROL, -10, 1);
    rest(&mut ctx, BOB, BUY, 9_900, 5); // something every kind could reach
    for flags in [RSV, RSV | RO] {
        for (ot, tif) in
            [(LIMIT, GTC), (LIMIT, IOC), (LIMIT, FOK), (LIMIT, PO), (MKT, GTC), (MKT, IOC)]
        {
            let price = if ot == MKT { 0 } else { 9_900 };
            rejected(&mut ctx, ALICE, &place(SELL, price, 1, ot, tif, flags), E_MODE);
        }
    }
    assert_eq!(nonce(&mut ctx, ALICE), 0, "a refused placement consumes no id");
}

/// T13 / §6.1: bit 1 and bits 4-7 are still unknown and refused, and the unknown-bit check runs
/// FIRST — a byte carrying both an unknown bit and the reserved mode reports the unknown bit.
/// `0x04` / `0x08` are now legal modes, not unknown bits.
#[test]
fn t13_unknown_bits_are_refused_before_the_mode_is_looked_at() {
    let mut ctx = fresh();
    for bit in [0x02u8, 0x10, 0x20, 0x40, 0x80] {
        rejected(&mut ctx, ALICE, &limit(BUY, 9_000, 1, GTC, bit), &e_unknown(bit));
        rejected(&mut ctx, ALICE, &limit(BUY, 9_000, 1, GTC, bit | RSV), &e_unknown(bit));
        rejected(&mut ctx, ALICE, &limit(BUY, 9_000, 1, GTC, bit | ET), &e_unknown(bit));
    }
    for mode in [ET, EB] {
        rest_with(&mut ctx, ALICE, BUY, 9_000, 1, GTC, mode);
    }
    assert_eq!(nonce(&mut ctx, ALICE), 2);
}

/// T12 on the batch path: the reserved mode is a per-item reject with reason 34 (not 255, M19). A
/// batch whose only item is refused writes NOTHING and logs nothing; in a mixed batch the refused
/// items consume no id and the others place as usual.
#[test]
fn t12_batch_item_with_the_reserved_mode_is_rejected_with_reason_34() {
    let mut ctx = fresh();
    let before = world(&mut ctx);
    let writes = ctx.perp_write_count();
    let (out, logs) = ok(&mut ctx, ALICE, &batch(vec![item(BUY, 9_000, 1, LIMIT, GTC, RSV)]));
    assert_eq!(batch_records(&out), vec![(TAG_REJECTED, [0u8; 32], B_STP_MODE)]);
    assert!(logs.is_empty(), "{:?}", names(&logs));
    assert_eq!(ctx.perp_write_count(), writes, "a fully-rejected batch wrote perp state");
    assert!(world(&mut ctx) == before);

    rest(&mut ctx, BOB, SELL, 10_000, 5);
    let (out, logs) = ok(
        &mut ctx,
        ALICE,
        &batch(vec![
            item(BUY, 9_000, 1, LIMIT, GTC, ET),
            item(BUY, 9_000, 1, LIMIT, GTC, RSV | RO),
            item(BUY, 10_000, 1, LIMIT, FOK, RSV),
            item(BUY, 0, 1, MKT, IOC, RSV),
            item(BUY, 9_000, 1, LIMIT, PO, RSV),
            item(BUY, 10_000, 1, LIMIT, IOC, EB),
        ]),
    );
    let (id0, id1) = (oid(ALICE, 0), oid(ALICE, 1));
    assert_eq!(
        batch_records(&out),
        vec![
            (TAG_ACCEPTED, id0, 0),
            (TAG_REJECTED, [0u8; 32], B_STP_MODE),
            (TAG_REJECTED, [0u8; 32], B_STP_MODE),
            (TAG_REJECTED, [0u8; 32], B_STP_MODE),
            (TAG_REJECTED, [0u8; 32], B_STP_MODE),
            (TAG_TERMINAL, id1, 0),
        ]
    );
    assert_eq!(evs::<OrderPlaced>(&logs).len(), 2, "a refused item emits no OrderPlaced");
    assert_eq!(nonce(&mut ctx, ALICE), 2);
    assert_eq!(trades(&logs), vec![(ALICE, BOB, 10_000, 1)]);
}

/// T12 on the signed paths, each in the SAME shape as an unknown-bit reject on that path (plan
/// §4): `placeOrderSigned` and `batchPlaceOrdersSigned` burn the signature first (so a resubmission
/// is a duplicate), `placeAlgoOrderSigned` validates before its burn (so it writes nothing).
#[test]
fn t12_signed_paths_refuse_the_reserved_mode_in_the_unknown_bit_shape() {
    // placeOrderSigned: measure the unknown-bit reject's write shape on a twin, then hold the
    // reserved mode to exactly that.
    let burn = {
        let mut twin = fresh();
        register_alice_key(&mut twin);
        let input = signed_place(&mut twin, BUY, 9_000, 1, GTC, 0x10, 0);
        let w = twin.perp_write_count();
        let out = run_perp_dex_call(&input, GAS, DAVE, U256::ZERO, false, &mut twin).unwrap();
        assert_eq!(revert_reason(&out), e_unknown(0x10));
        twin.perp_write_count() - w
    };
    assert!(burn > 0, "the signed path burns before the placement can reject");
    let mut ctx = fresh();
    register_alice_key(&mut ctx);
    for (salt, flags) in [(1u8, RSV), (2, RSV | RO)] {
        let input = signed_place(&mut ctx, BUY, 9_000, 1, GTC, flags, salt);
        rejected_burning(&mut ctx, DAVE, &input, E_MODE, burn);
        rejected_burning(&mut ctx, DAVE, &input, E_DUP_SIG, 0);
    }

    // batchPlaceOrdersSigned: the whole call succeeds (the burn), the item is reason 34.
    let input = signed_batch(&mut ctx, vec![item(BUY, 9_000, 1, LIMIT, GTC, RSV)]);
    let before = world(&mut ctx);
    let (out, logs) = ok(&mut ctx, DAVE, &input);
    assert_eq!(batch_records(&out), vec![(TAG_REJECTED, [0u8; 32], B_STP_MODE)]);
    assert!(logs.is_empty());
    assert!(world(&mut ctx) == before);

    // placeAlgoOrderSigned: validated BEFORE the burn — zero writes, and resubmitting gets the same
    // reject again rather than "duplicate".
    seed_pos(&mut ctx, ALICE, 10, 1);
    seed_pos(&mut ctx, CAROL, -10, 1);
    let unknown_writes = {
        let input = signed_algo(&mut ctx, SELL, 9_500, 1, 0x10 | RO);
        let w = ctx.perp_write_count();
        let out = run_perp_dex_call(&input, GAS, DAVE, U256::ZERO, false, &mut ctx).unwrap();
        assert_eq!(revert_reason(&out), format!("placeAlgoOrderSigned: {E_ALGO_UNKNOWN}"));
        ctx.perp_write_count() - w
    };
    let input = signed_algo(&mut ctx, SELL, 9_500, 1, RSV | RO);
    rejected_burning(&mut ctx, DAVE, &input, E_ALGO_MODE_SIGNED, unknown_writes);
    rejected_burning(&mut ctx, DAVE, &input, E_ALGO_MODE_SIGNED, unknown_writes);
    assert_eq!(unknown_writes, 0);
}

/// T12 on `placeAlgoOrder` and through `asSubAccount`. The mode check sits right after the
/// unknown-bit check (§1.3), so it fires even on a byte the reduce-only scope check would also
/// refuse, and an unknown bit still wins over it.
#[test]
fn t12_place_algo_order_and_as_sub_account_refuse_the_reserved_mode() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 10, 1);
    seed_pos(&mut ctx, CAROL, -10, 1);
    for flags in [RSV | RO, RSV, RSV | ALGO_FLAG_CLOSE_POSITION] {
        let qty = if flags & ALGO_FLAG_CLOSE_POSITION != 0 { 0 } else { 1 };
        rejected(&mut ctx, ALICE, &algo(SELL, STOP_MARKET, 9_500, 0, qty, 0, flags), E_ALGO_MODE);
        rejected(&mut ctx, ALICE, &algo(SELL, STOP, 9_500, 9_400, 1, GTC, flags | RO), E_ALGO_MODE);
    }
    for flags in [0x10 | RO, 0x10 | RSV | RO, 0x80 | RSV] {
        rejected(&mut ctx, ALICE, &algo(SELL, STOP_MARKET, 9_500, 0, 1, 0, flags), E_ALGO_UNKNOWN);
    }
    assert_eq!(nonce(&mut ctx, ALICE), 0);

    let (out, _) = ok(&mut ctx, ALICE, &createSubAccountCall { name: "desk".into() }.abi_encode());
    let sub = createSubAccountCall::abi_decode_returns(&out.bytes).unwrap();
    EXTRA_WATCHED.with(|e| e.borrow_mut().push(sub));
    let wrapped = asSubAccountCall {
        subAccount: sub,
        data: limit(BUY, 9_000, 1, GTC, RSV).into(),
    }
    .abi_encode();
    rejected(&mut ctx, ALICE, &wrapped, E_MODE);
    EXTRA_WATCHED.with(|e| e.borrow_mut().clear());
}

/// T14 / §3.3: every echo equals the byte that was sent, for all three modes × {GTC rest, PostOnly
/// rest, IOC, FOK, market} × {plain, reduce-only}, and for a conditional. `0b00` means
/// EXPIRE_MAKER, so an echo site that forgot the stored mode would silently print `0` — the ET / EB
/// rows catch it.
#[test]
fn t14_every_echo_carries_the_sent_mode() {
    for mode in MODES {
        for ro in [0, RO] {
            let f = mode | ro;
            let mut ctx = fresh();
            seed_pos(&mut ctx, ALICE, 50, 1);
            seed_pos(&mut ctx, CAROL, -50, 1);
            rest(&mut ctx, BOB, BUY, 9_900, 100);
            for tif in [GTC, PO] {
                let (out, logs) = ok(&mut ctx, ALICE, &limit(SELL, 10_500, 1, tif, f));
                let id = order_id_of(&out);
                assert_eq!(evs::<OrderPlaced>(&logs)[0].flags, f, "OrderPlaced {f:#06b} tif {tif}");
                assert_eq!(evs::<OrderRested>(&logs)[0].flags, f, "OrderRested {f:#06b} tif {tif}");
                assert_eq!(get_order(&mut ctx, id).unwrap(), (0, S_OPEN, f), "getOrder");
                assert_eq!(open_order_flags(&mut ctx, ALICE, id), f, "getOpenOrders");
            }
            for (ot, tif, px) in [(LIMIT, IOC, 9_900), (LIMIT, FOK, 9_900), (MKT, GTC, 0)] {
                let (_, logs) = ok(&mut ctx, ALICE, &place(SELL, px, 1, ot, tif, f));
                assert_eq!(evs::<OrderPlaced>(&logs)[0].flags, f, "{ot}/{tif}");
                assert_eq!(trades(&logs), vec![(ALICE, BOB, 9_900, 1)]);
            }
            // A conditional must be reduce-only in this build.
            let af = mode | RO;
            let (out, logs) = ok(&mut ctx, ALICE, &algo(SELL, STOP_MARKET, 9_500, 0, 1, 0, af));
            assert_eq!(evs::<AlgoOrderPlaced>(&logs)[0].flags, af);
            let id = placeAlgoOrderCall::abi_decode_returns(&out.bytes).unwrap();
            let got = view(&mut ctx, &getAlgoOrderCall { algoId: id }.abi_encode());
            let r = getAlgoOrderCall::abi_decode_returns(&got.bytes).unwrap();
            assert_eq!(r.selfTradePreventionMode, mode >> 2);
            assert!(r.reduceOnly && !r.closePosition);
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §2 STP in the walk (§2.2 / §2.3 table), every case on both taker sides
// ═════════════════════════════════════════════════════════════════════════════════════════════

const OP: &str = "OrderPlaced";
const OR: &str = "OrderRested";
const OC: &str = "OrderCancelled";
const OE: &str = "OrderExpired";
const TR: &str = "Trade";
const ABC: &str = "AccountBalanceChanged";
const PC: &str = "PositionChanged";

/// The three taker kinds STP applies to: `(orderType, tif)`.
const STP_KINDS: [(u8, u8); 3] = [(LIMIT, GTC), (LIMIT, IOC), (MKT, GTC)];

/// A taker of `kind` on `d`'s side for `qty`, limit-priced at maker level `k` (ignored for market).
fn taker(d: Dir, kind: (u8, u8), k: u64, qty: u64, flags: u8) -> Vec<u8> {
    let price = if kind.0 == MKT { 0 } else { d.lvl(k) };
    place(d.taker(), price, qty, kind.0, kind.1, flags)
}

fn t1_setup(d: Dir) -> (TestCtx, [u8; 32]) {
    let mut ctx = fresh();
    set_ts(&mut ctx, 10);
    rest(&mut ctx, BOB, d.taker(), d.passive(), 1);
    let m = rest_with(&mut ctx, ALICE, d.maker(), d.lvl(0), 5, PO, 0);
    set_ts(&mut ctx, 20);
    (ctx, m)
}

/// T1 (+ M1, M5, M23, M26): a `flags = 0` GTC that meets ONLY its owner's resting PostOnly — whose
/// size exceeds the taker's — cancels that maker WHOLE with reason 9, trades nothing, publishes no
/// account row, removes the level, refreshes the BBO, samples the mid exactly as the owner's own
/// cancel would, leaves `last_traded` alone, and rests the taker in full. The end state is
/// bit-identical to the owner cancelling the maker herself and then placing the same order.
#[test]
fn t1_default_mode_cancels_the_owners_resting_post_only_whole_and_rests_the_taker() {
    for d in DIRS {
        let (mut ctx, m) = t1_setup(d);
        let input = limit(d.taker(), d.lvl(0), 2, GTC, EM);
        let (out, logs) = ok(&mut ctx, ALICE, &input);
        let t = order_id_of(&out);
        assert_eq!(seq(&logs), vec![(OP, ALICE), (OC, ALICE), (OR, ALICE)], "{d:?}");
        assert_eq!(cancels(&logs), vec![(ALICE, m, C_STP)]);
        let r = &evs::<OrderRested>(&logs)[0];
        assert_eq!((r.orderId.0, r.side, r.price, r.quantity), (t, d.taker(), d.lvl(0), 2));
        assert_eq!(order(&mut ctx, m), None, "the maker's WHOLE remainder (5 > 2) is gone");
        assert!(open_ids(&mut ctx, ALICE, d.maker()).is_empty());
        assert_eq!(open_ids(&mut ctx, ALICE, d.taker()), vec![t]);
        assert_eq!(prices(&mut ctx, d.maker()), Vec::<u64>::new());
        assert_eq!(level(&mut ctx, d.maker(), d.lvl(0)), (vec![], 0));
        let (bid, ask) = bbo(&mut ctx);
        if d.taker() == BUY {
            assert_eq!((bid, ask), (d.lvl(0), 0));
        } else {
            assert_eq!((bid, ask), (0, d.lvl(0)));
        }
        assert_eq!(last_traded(&mut ctx), 0);
        // ts 10: BOB's quote (ALICE's maker at the same ts is not sampled); ts 20: the STP cancel —
        // the first best-quote change at that ts — leaves only BOB's quote.
        assert_eq!(mid_window(&mut ctx), (20, d.passive(), 2));
        assert_eq!(amount(&mut ctx, ALICE), 0);

        let (mut twin, tm) = t1_setup(d);
        let (_, clogs) = ok(&mut twin, ALICE, &cancel(tm));
        assert_eq!(cancels(&clogs), vec![(ALICE, tm, C_USER)]);
        let (tout, _) = ok(&mut twin, ALICE, &input);
        assert_eq!(order_id_of(&tout), t);
        assert!(world(&mut twin) == world(&mut ctx), "{d:?}: STP != cancel-then-place");
    }
}

/// T2 (§2.3 FIFO, UNVERIFIED vs Binance but the plan's choice): within one level, a third party
/// queued AHEAD of the owner fills first; the owner's order is cancelled; the third party BEHIND
/// it fills next; the walk then goes on to the next level.
#[test]
fn t2_a_third_party_ahead_in_the_same_level_fills_first() {
    for d in DIRS {
        let mut ctx = fresh();
        let ids = book(&mut ctx, d, &[&[(BOB, 1), (ALICE, 2), (CAROL, 1)], &[(DAVE, 5)]]);
        let (_, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(1), 3, GTC, EM));
        assert_eq!(
            seq(&logs),
            vec![
                (OP, ALICE),
                (TR, BOB),
                (ABC, BOB),
                (PC, BOB),
                (OC, ALICE),
                (TR, CAROL),
                (ABC, CAROL),
                (PC, CAROL),
                (TR, DAVE),
                (ABC, DAVE),
                (PC, DAVE),
                (ABC, ALICE),
                (PC, ALICE),
            ],
            "{d:?}"
        );
        assert_eq!(cancels(&logs), vec![(ALICE, ids[0][1], C_STP)]);
        assert_eq!(
            trades(&logs),
            vec![
                (ALICE, BOB, d.lvl(0), 1),
                (ALICE, CAROL, d.lvl(0), 1),
                (ALICE, DAVE, d.lvl(1), 1),
            ]
        );
        assert_eq!(prices(&mut ctx, d.maker()), vec![d.lvl(1)]);
        assert_eq!(level(&mut ctx, d.maker(), d.lvl(1)), (vec![ids[1][0]], 1));
        assert_eq!(amount(&mut ctx, ALICE), 3 * d.sign());
    }
}

/// T3 (+ M4): EXPIRE_MAKER cancels the owner's orders across THREE levels without consuming any of
/// the taker's quantity, fills the third parties among and beyond them, and rests the remainder —
/// in exactly the §3.5 stream order.
#[test]
fn t3_expire_maker_walks_through_three_levels_of_own_orders_without_spending_quantity() {
    for d in DIRS {
        let mut ctx = fresh();
        let ids = book(
            &mut ctx,
            d,
            &[&[(ALICE, 1)], &[(ALICE, 2)], &[(BOB, 1), (ALICE, 1)], &[(CAROL, 3)]],
        );
        let (out, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(3), 5, GTC, EM));
        let t = order_id_of(&out);
        assert_eq!(
            seq(&logs),
            vec![
                (OP, ALICE),
                (OC, ALICE),
                (OC, ALICE),
                (TR, BOB),
                (ABC, BOB),
                (PC, BOB),
                (OC, ALICE),
                (TR, CAROL),
                (ABC, CAROL),
                (PC, CAROL),
                (ABC, ALICE),
                (PC, ALICE),
                (OR, ALICE),
            ],
            "{d:?}"
        );
        assert_eq!(
            cancels(&logs),
            vec![(ALICE, ids[0][0], C_STP), (ALICE, ids[1][0], C_STP), (ALICE, ids[2][1], C_STP)]
        );
        assert_eq!(
            trades(&logs),
            vec![(ALICE, BOB, d.lvl(2), 1), (ALICE, CAROL, d.lvl(3), 3)]
        );
        let r = &evs::<OrderRested>(&logs)[0];
        assert_eq!((r.price, r.quantity), (d.lvl(3), 1), "5 - 1 - 3: no unit spent on a cancel");
        assert_eq!(get_order(&mut ctx, t), Ok((4, S_PARTIAL, EM)));
        assert_eq!(prices(&mut ctx, d.maker()), Vec::<u64>::new(), "every level closed");
        for k in 0..4 {
            assert_eq!(level(&mut ctx, d.maker(), d.lvl(k)), (vec![], 0), "level {k}");
        }
        for id in [ids[0][0], ids[1][0], ids[2][1]] {
            assert_eq!(order(&mut ctx, id), None);
        }
        assert!(open_ids(&mut ctx, ALICE, d.maker()).is_empty());
        assert_eq!(amount(&mut ctx, ALICE), 4 * d.sign());
    }
}

/// T4 (+ M8, M9, M10): an EXPIRE_TAKER GTC keeps the fill before the stop, EXPIRES the rest with
/// reason 9 instead of resting it, leaves no order record, and leaves the owner's maker in its FIFO
/// slot with the level's count untouched; nothing deeper is touched.
#[test]
fn t4_expire_taker_gtc_keeps_prior_fills_expires_the_rest_and_leaves_the_maker_in_place() {
    for d in DIRS {
        let mut ctx = fresh();
        let ids = book(&mut ctx, d, &[&[(BOB, 1)], &[(ALICE, 2), (DAVE, 1)], &[(CAROL, 5)]]);
        let (out, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(2), 5, GTC, ET));
        let t = order_id_of(&out);
        assert_eq!(
            seq(&logs),
            vec![
                (OP, ALICE),
                (TR, BOB),
                (ABC, BOB),
                (PC, BOB),
                (ABC, ALICE),
                (PC, ALICE),
                (OE, ALICE),
            ],
            "{d:?}"
        );
        assert_eq!(expiry(&logs), (t, 1, 4, X_STP));
        assert_eq!(get_order(&mut ctx, t), Err(E_NOT_FOUND.to_string()));
        assert_eq!(order(&mut ctx, t), None);
        assert!(open_ids(&mut ctx, ALICE, d.taker()).is_empty(), "it never rested");
        assert_eq!(level(&mut ctx, d.maker(), d.lvl(1)), (vec![ids[1][0], ids[1][1]], 2));
        assert_eq!(level(&mut ctx, d.maker(), d.lvl(2)), (vec![ids[2][0]], 1));
        let own = order(&mut ctx, ids[1][0]).unwrap();
        assert_eq!((own.quantity, own.filled, own.status as u8), (2, 0, S_OPEN));
        assert_eq!(prices(&mut ctx, d.maker()), vec![d.lvl(1), d.lvl(2)]);
        assert_eq!((amount(&mut ctx, CAROL), amount(&mut ctx, DAVE)), (0, 0));
        assert_eq!(amount(&mut ctx, ALICE), d.sign());
    }
}

/// T5 (+ M7): the owner's order is the LAST id of its level and a third party waits one level
/// deeper. EXPIRE_TAKER and EXPIRE_BOTH must stop the WHOLE walk there — the deeper level is never
/// reached — for a GTC, an IOC and a market taker alike.
#[test]
fn t5_the_stop_holds_when_the_own_order_is_the_last_id_of_its_level() {
    for d in DIRS {
        for mode in [ET, EB] {
            for kind in STP_KINDS {
                let mut ctx = fresh();
                let ids = book(&mut ctx, d, &[&[(BOB, 1), (ALICE, 1)], &[(CAROL, 5)]]);
                let (out, logs) = ok(&mut ctx, ALICE, &taker(d, kind, 1, 4, mode));
                let t = order_id_of(&out);
                let tag = format!("{d:?} mode {mode:#06b} kind {kind:?}");
                assert_eq!(trades(&logs), vec![(ALICE, BOB, d.lvl(0), 1)], "{tag}");
                assert_eq!(expiry(&logs), (t, 1, 3, X_STP), "{tag}");
                assert_eq!(amount(&mut ctx, CAROL), 0, "{tag}: the next level was reached");
                assert_eq!(level(&mut ctx, d.maker(), d.lvl(1)), (vec![ids[1][0]], 1));
                if mode == ET {
                    assert!(cancels(&logs).is_empty(), "{tag}");
                    assert_eq!(level(&mut ctx, d.maker(), d.lvl(0)), (vec![ids[0][1]], 1));
                    assert_eq!(prices(&mut ctx, d.maker()), vec![d.lvl(0), d.lvl(1)]);
                } else {
                    assert_eq!(cancels(&logs), vec![(ALICE, ids[0][1], C_STP)], "{tag}");
                    assert_eq!(level(&mut ctx, d.maker(), d.lvl(0)), (vec![], 0));
                    assert_eq!(prices(&mut ctx, d.maker()), vec![d.lvl(1)]);
                }
                assert!(open_ids(&mut ctx, ALICE, d.taker()).is_empty(), "{tag}: rested");
            }
        }
    }
}

/// The keys written since the previous call, drained from the typed live store's dirty set — the
/// exact key set the block commitment folds. A same-value write IS in it (the delta does not filter
/// them), so a redundant write changes the commitment even when no stored value moves. Only the
/// dirty flags go; every value stays, so the context remains usable.
fn drain_written(ctx: &mut TestCtx) -> Vec<B256> {
    ctx.live().take_delta().unwrap().into_iter().map(|(k, _)| k).collect()
}

/// The storage key of the book level on `side` at `price`.
fn level_key(side: u8, price: u64) -> B256 {
    if side == BUY {
        bid_level_key(MARKET_ID, price)
    } else {
        ask_level_key(MARKET_ID, price)
    }
}

/// T5b (+ M7b): T5 checks that nothing beyond the stop FILLS, and a stop written as "set a flag,
/// test it at the top of the `while`" passes that. When the own order is the level's LAST id, that
/// form lets the `while` end normally, so the `for` loads the NEXT level, meets the flag at the top
/// of its `while` and closes it untouched: no fill, but a redundant level write in the block delta,
/// i.e. a different commitment. The stop must be final: the walk writes the level it stopped on
/// and NOTHING beyond it. EXPIRE_TAKER (the own order stays; its level is re-saved as it was) and
/// EXPIRE_BOTH (it is cancelled, its level removed and the best quote refreshed from the price
/// index, which READS the next price but must not rewrite its level), for every STP taker kind on
/// both sides.
#[test]
fn t5b_an_stp_stop_on_a_levels_last_id_writes_no_level_beyond_it() {
    for d in DIRS {
        for mode in [ET, EB] {
            for kind in STP_KINDS {
                let mut ctx = fresh();
                let own = rest(&mut ctx, ALICE, d.maker(), d.lvl(0), 1);
                let third = rest(&mut ctx, BOB, d.maker(), d.lvl(1), 1);
                let next_before = level(&mut ctx, d.maker(), d.lvl(1));
                let _ = drain_written(&mut ctx);

                let (out, logs) = ok(&mut ctx, ALICE, &taker(d, kind, 1, 1, mode));
                let t = order_id_of(&out);
                let tag = format!("{d:?} mode {mode:#06b} kind {kind:?}");
                assert_eq!(expiry(&logs), (t, 0, 1, X_STP), "{tag}");
                assert!(trades(&logs).is_empty(), "{tag}: nothing trades");
                if mode == EB {
                    assert_eq!(cancels(&logs), vec![(ALICE, own, C_STP)], "{tag}");
                } else {
                    assert!(cancels(&logs).is_empty(), "{tag}");
                }

                let written = drain_written(&mut ctx);
                assert!(
                    written.contains(&level_key(d.maker(), d.lvl(0))),
                    "{tag}: fixture — the level the walk stopped on is written"
                );
                assert!(
                    !written.contains(&level_key(d.maker(), d.lvl(1))),
                    "{tag}: the STP stop is final, yet the level BEYOND it ({}) was rewritten",
                    d.lvl(1)
                );
                assert_eq!(level(&mut ctx, d.maker(), d.lvl(1)), next_before, "{tag}");
                assert!(order(&mut ctx, third).is_some(), "{tag}: the third party is untouched");
            }
        }
    }
}

/// T6 (+ M6): EXPIRE_BOTH where the owner's order is the level's ONLY live order: the price leaves
/// the index, the BBO moves to the next level, and the taker expires untouched (0 filled, reason
/// 9).
#[test]
fn t6_expire_both_on_the_last_live_order_removes_the_price_and_refreshes_the_best() {
    for d in DIRS {
        for kind in STP_KINDS {
            let mut ctx = fresh();
            let ids = book(&mut ctx, d, &[&[(ALICE, 2)], &[(CAROL, 3)]]);
            let (out, logs) = ok(&mut ctx, ALICE, &taker(d, kind, 1, 1, EB));
            let t = order_id_of(&out);
            assert_eq!(seq(&logs), vec![(OP, ALICE), (OC, ALICE), (OE, ALICE)], "{d:?} {kind:?}");
            assert_eq!(cancels(&logs), vec![(ALICE, ids[0][0], C_STP)]);
            assert_eq!(expiry(&logs), (t, 0, 1, X_STP));
            assert_eq!(prices(&mut ctx, d.maker()), vec![d.lvl(1)]);
            assert_eq!(level(&mut ctx, d.maker(), d.lvl(0)), (vec![], 0));
            let (bid, ask) = bbo(&mut ctx);
            assert_eq!(if d.taker() == BUY { ask } else { bid }, d.lvl(1));
            assert_eq!(amount(&mut ctx, CAROL), 0);
        }
    }
}

/// T7 (+ M6): EXPIRE_BOTH where third parties queue BEHIND the owner: the count drops by exactly
/// one and their order is preserved; neither is filled.
#[test]
fn t7_expire_both_keeps_the_queue_behind_the_own_order_in_order() {
    for d in DIRS {
        let mut ctx = fresh();
        let ids = book(&mut ctx, d, &[&[(ALICE, 2), (BOB, 1), (CAROL, 1)]]);
        let (out, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(0), 1, GTC, EB));
        assert_eq!(cancels(&logs), vec![(ALICE, ids[0][0], C_STP)]);
        assert!(trades(&logs).is_empty());
        assert_eq!(expiry(&logs), (order_id_of(&out), 0, 1, X_STP));
        assert_eq!(level(&mut ctx, d.maker(), d.lvl(0)), (vec![ids[0][1], ids[0][2]], 2));
        assert_eq!((amount(&mut ctx, BOB), amount(&mut ctx, CAROL)), (0, 0));
    }
}

/// T8 (+ M11): an IOC / market taker under EXPIRE_MAKER that cancels its owner's order and then
/// runs out of price or book expires with reason 0 — STP did not stop it (Binance scenario E).
#[test]
fn t8_expire_maker_ioc_and_market_that_run_out_expire_with_reason_0() {
    for d in DIRS {
        // IOC: stopped by its limit price at level 1; CAROL at level 2 untouched.
        let mut ctx = fresh();
        let ids = book(&mut ctx, d, &[&[(ALICE, 1)], &[(BOB, 1)], &[(CAROL, 1)]]);
        let (out, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(1), 3, IOC, EM));
        assert_eq!(cancels(&logs), vec![(ALICE, ids[0][0], C_STP)]);
        assert_eq!(trades(&logs), vec![(ALICE, BOB, d.lvl(1), 1)]);
        assert_eq!(expiry(&logs), (order_id_of(&out), 1, 2, X_TIF), "{d:?} IOC");
        assert_eq!(amount(&mut ctx, CAROL), 0);

        // Market: runs out of book.
        let mut ctx = fresh();
        let ids = book(&mut ctx, d, &[&[(ALICE, 1)], &[(BOB, 1)], &[(CAROL, 1)]]);
        let (out, logs) = ok(&mut ctx, ALICE, &market(d.taker(), 3, EM));
        assert_eq!(cancels(&logs), vec![(ALICE, ids[0][0], C_STP)]);
        assert_eq!(trades(&logs), vec![(ALICE, BOB, d.lvl(1), 1), (ALICE, CAROL, d.lvl(2), 1)]);
        assert_eq!(expiry(&logs), (order_id_of(&out), 2, 1, X_TIF), "{d:?} market");
        assert_eq!(prices(&mut ctx, d.maker()), Vec::<u64>::new());
    }
}

/// T9 (+ M11, D9): IOC / market under EXPIRE_TAKER or EXPIRE_BOTH stop at the owner's order with
/// reason 9 (fills before it stand, nothing deeper is touched). With nothing filled, the stream is
/// exactly `OrderPlaced · [OrderCancelled(9)] · OrderExpired(0, qty, 9)` — no account row (§3.5).
#[test]
fn t9_expire_taker_and_both_ioc_and_market_stop_with_reason_9() {
    for d in DIRS {
        for mode in [ET, EB] {
            for kind in [(LIMIT, IOC), (MKT, GTC)] {
                let tag = format!("{d:?} {mode:#06b} {kind:?}");
                let mut ctx = fresh();
                let ids = book(&mut ctx, d, &[&[(BOB, 1)], &[(ALICE, 1)], &[(CAROL, 5)]]);
                let (out, logs) = ok(&mut ctx, ALICE, &taker(d, kind, 2, 4, mode));
                assert_eq!(trades(&logs), vec![(ALICE, BOB, d.lvl(0), 1)], "{tag}");
                assert_eq!(expiry(&logs), (order_id_of(&out), 1, 3, X_STP), "{tag}");
                assert_eq!(amount(&mut ctx, CAROL), 0, "{tag}");
                let want = if mode == EB { vec![(ALICE, ids[1][0], C_STP)] } else { vec![] };
                assert_eq!(cancels(&logs), want, "{tag}");

                // Zero fill.
                let mut ctx = fresh();
                let ids = book(&mut ctx, d, &[&[(ALICE, 1)], &[(CAROL, 5)]]);
                let (out, logs) = ok(&mut ctx, ALICE, &taker(d, kind, 1, 2, mode));
                let want = if mode == EB { vec![OP, OC, OE] } else { vec![OP, OE] };
                assert_eq!(names(&logs), want, "{tag} zero fill");
                assert_eq!(expiry(&logs), (order_id_of(&out), 0, 2, X_STP));
                if mode == ET {
                    assert_eq!(level(&mut ctx, d.maker(), d.lvl(0)), (vec![ids[0][0]], 1));
                }
                assert_eq!(amount(&mut ctx, CAROL), 0);
            }
        }
    }
}

/// T10 (+ M12): a FOK taker is NOT subject to STP (D3) under any mode: it self-trades — a `Trade`
/// with maker == taker, both legs charged their fee, two account rows for the owner — and the
/// owner's own liquidity counts toward its fill-or-kill feasibility.
#[test]
fn t10_fok_still_self_trades_under_every_mode() {
    for d in DIRS {
        for mode in MODES {
            let mut ctx = fresh();
            set_fees(&mut ctx, ALICE, 2, 5);
            let ids = book(&mut ctx, d, &[&[(ALICE, 2)]]);
            let fees0 = fee_total(&mut ctx);
            let (_, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(0), 2, FOK, mode));
            assert_eq!(trades(&logs), vec![(ALICE, ALICE, d.lvl(0), 2)], "{d:?} {mode:#06b}");
            let tr = &evs::<Trade>(&logs)[0];
            // 2 × $100 = 200_000_000; taker 5 bps, maker 2 bps.
            assert_eq!((tr.takerFee, tr.makerFee), (100_000, 40_000));
            assert_eq!(fee_total(&mut ctx) - fees0, 140_000);
            assert!(cancels(&logs).is_empty());
            let alice_rows = evs::<AccountBalanceChanged>(&logs)
                .iter()
                .filter(|e| e.user == ALICE)
                .count();
            assert_eq!(alice_rows, 2, "a FOK self-trade publishes both legs");
            assert_eq!(order(&mut ctx, ids[0][0]), None);
            assert_eq!(amount(&mut ctx, ALICE), 0);

            // Own liquidity counts toward feasibility: 1 own + 1 third party fill a FOK of 2.
            let mut ctx = fresh();
            book(&mut ctx, d, &[&[(ALICE, 1)], &[(BOB, 1)]]);
            let (_, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(1), 2, FOK, mode));
            assert_eq!(
                trades(&logs),
                vec![(ALICE, ALICE, d.lvl(0), 1), (ALICE, BOB, d.lvl(1), 1)]
            );
        }
    }
}

/// T11 (+ M13): a PostOnly taker is not subject to STP either: crossing its owner's order is
/// refused like any other cross, under every mode, writing nothing.
#[test]
fn t11_post_only_crossing_its_owner_is_refused_under_every_mode() {
    for d in DIRS {
        let mut ctx = fresh();
        book(&mut ctx, d, &[&[(ALICE, 1)]]);
        for mode in MODES {
            rejected(&mut ctx, ALICE, &limit(d.taker(), d.lvl(0), 1, PO, mode), E_PO_CROSS);
            rejected(&mut ctx, ALICE, &limit(d.taker(), d.lvl(1), 1, PO, mode), E_PO_CROSS);
        }
    }
}

/// B1 (Appendix B): there is NO per-order STP cancel limit (D10 removed). One minimum-size taker
/// cancels 150 of its owner's orders across three levels — far past the old 64 — in FIFO order,
/// then fills the third party behind them.
#[test]
fn b1_expire_maker_cancels_every_own_order_on_the_path_with_no_per_order_limit() {
    for d in DIRS {
        let mut ctx = fresh();
        let mut own = Vec::new();
        for k in 0..3 {
            for _ in 0..50 {
                own.push(rest(&mut ctx, ALICE, d.maker(), d.lvl(k), 1));
            }
        }
        rest(&mut ctx, CAROL, d.maker(), d.lvl(3), 1);
        let (_, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(3), 1, GTC, EM));
        let want: Vec<_> = own.iter().map(|&id| (ALICE, id, C_STP)).collect();
        assert_eq!(cancels(&logs), want, "{d:?}");
        assert_eq!(trades(&logs), vec![(ALICE, CAROL, d.lvl(3), 1)]);
        assert!(open_ids(&mut ctx, ALICE, d.maker()).is_empty());
        assert_eq!(prices(&mut ctx, d.maker()), Vec::<u64>::new());
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §3 Interactions (§2.4): prints, margin, reduce-only, band
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// T17 (+ M22): a match STP prevents prints nothing — no `Trade`, no fee into the market total, no
/// `last_traded`, and no trade id consumed (the NEXT real trade gets the id the counter already
/// held) — for every mode and every STP kind, with non-zero fees on everyone.
#[test]
fn t17_a_prevented_match_moves_no_last_traded_fee_or_trade_id() {
    for d in DIRS {
        for mode in MODES {
            for kind in STP_KINDS {
                let tag = format!("{d:?} {mode:#06b} {kind:?}");
                let mut ctx = fresh();
                for u in [ALICE, BOB, CAROL, DAVE] {
                    set_fees(&mut ctx, u, 2, 5);
                }
                // One real print: trade id 0, last_traded = lvl(0).
                rest(&mut ctx, BOB, d.maker(), d.lvl(0), 1);
                let (_, logs) = ok(&mut ctx, CAROL, &limit(d.taker(), d.lvl(0), 1, IOC, 0));
                assert_eq!(evs::<Trade>(&logs)[0].tradeId, 0);
                let fees0 = fee_total(&mut ctx);
                assert_eq!(last_traded(&mut ctx), d.lvl(0));
                let alice0 = storage::load_account(&mut ctx, ALICE).unwrap();
                let admin0 = storage::load_account(&mut ctx, ADMIN).unwrap();

                let own = rest(&mut ctx, ALICE, d.maker(), d.lvl(1), 2);
                let (_, logs) = ok(&mut ctx, ALICE, &taker(d, kind, 1, 1, mode));
                assert!(trades(&logs).is_empty(), "{tag}");
                assert_eq!(
                    cancels(&logs),
                    if mode == ET { vec![] } else { vec![(ALICE, own, C_STP)] },
                    "{tag}"
                );
                assert_eq!(last_traded(&mut ctx), d.lvl(0), "{tag}");
                assert_eq!(fee_total(&mut ctx), fees0, "{tag}");
                let money = |a: &UserAccount| (a.perp_wallet_balance, a.total_position_margin);
                let alice1 = storage::load_account(&mut ctx, ALICE).unwrap();
                let admin1 = storage::load_account(&mut ctx, ADMIN).unwrap();
                assert_eq!(money(&alice1), money(&alice0), "{tag}");
                assert_eq!(money(&admin1), money(&admin0), "{tag}");

                // The next real print gets trade id 1.
                rest(&mut ctx, DAVE, d.maker(), d.lvl(2), 1);
                let (_, logs) = ok(&mut ctx, CAROL, &limit(d.taker(), d.lvl(2), 1, IOC, 0));
                assert_eq!(evs::<Trade>(&logs)[0].tradeId, 1, "{tag}");
            }
        }
    }
}

/// Sets `user`'s wallet so that `wallet − Σ ooIM` is exactly `avail`.
fn set_available(ctx: &mut TestCtx, user: Address, avail: i64) {
    let w = storage::load_account(ctx, user).unwrap().perp_wallet_balance;
    let now = available(ctx, user);
    set_wallet(ctx, user, w - now + avail);
    assert_eq!(available(ctx, user), avail);
}

/// T18: an EXPIRE_MAKER GTC whose walk cancels the owner's crossing order and whose remainder then
/// cannot be afforded is refused WHOLE: the STP cancel is undone with everything else — the owner's
/// order is still there — and nothing is written or logged. (An own order on the taker's side keeps
/// the pre-walk filter out of it, so the refusal really comes after the walk.)
#[test]
fn t18_a_refused_rest_after_an_stp_cancel_reverts_the_cancel_too() {
    for d in DIRS {
        let mut ctx = fresh();
        rest(&mut ctx, ALICE, d.taker(), d.passive(), 1);
        let own = rest(&mut ctx, ALICE, d.maker(), d.lvl(0), 1);
        set_available(&mut ctx, ALICE, 1_000_000);
        let _ = take_logs(&mut ctx);
        rejected(&mut ctx, ALICE, &limit(d.taker(), d.lvl(0), 3, GTC, EM), E_MARGIN);
        assert_eq!(level(&mut ctx, d.maker(), d.lvl(0)), (vec![own], 1), "{d:?}");
        assert!(order(&mut ctx, own).is_some());
    }
}

fn t23_setup(d: Dir, own_qty: u64, wallet: i64) -> (TestCtx, [u8; 32]) {
    let mut ctx = fresh();
    let own = rest(&mut ctx, ALICE, d.maker(), d.lvl(0), own_qty);
    set_wallet(&mut ctx, ALICE, wallet);
    let _ = take_logs(&mut ctx);
    (ctx, own)
}

/// T23 (§2.4 pre-filter): under EXPIRE_MAKER the pre-walk affordability filter never refuses an
/// order the full path would admit. Pinned at the exact boundary — a GTC of 3 at $100 (leverage 1,
/// flat) needs exactly `3 × 100_000_000` once STP has cancelled the owner's crossing order — and
/// held to the outcome of the owner cancelling that order herself and then placing the same GTC:
/// admitted at the boundary with a bit-identical end state, refused one quote unit below it —
/// including when the owner's crossing order is larger than the GTC and she cannot currently
/// afford it.
#[test]
fn t23_the_pre_filter_never_over_rejects_an_expire_maker_order() {
    for d in DIRS {
        // The GTC of 3 needs exactly 3 × $100 once the owner's crossing order is gone, whatever
        // that order's size. With an own order of 5 the owner is UNDERWATER before the call
        // (available = 3 − 5 = −$200), so the order is affordable only because STP releases that
        // order.
        let need = 3 * UNIT_VALUE;
        for own_qty in [1u64, 5] {
            let tag = format!("{d:?} own {own_qty}");
            for wallet in [need, need - 1] {
                // The full path, by hand.
                let (mut manual, m) = t23_setup(d, own_qty, wallet);
                ok(&mut manual, ALICE, &cancel(m));
                let input = limit(d.taker(), d.lvl(0), 3, GTC, EM);
                let _ = take_logs(&mut manual);
                let w = manual.perp_write_count();
                let out =
                    run_perp_dex_call(&input, GAS, ALICE, U256::ZERO, false, &mut manual).unwrap();
                let manual_ok = !out.reverted;
                if !manual_ok {
                    assert_eq!(revert_reason(&out), E_MARGIN, "{tag}");
                    assert_eq!(manual.perp_write_count(), w);
                }
                // Through STP.
                let (mut ctx, own) = t23_setup(d, own_qty, wallet);
                if manual_ok {
                    let (_, logs) = ok(&mut ctx, ALICE, &input);
                    assert_eq!(cancels(&logs), vec![(ALICE, own, C_STP)], "{tag}");
                    assert_eq!(evs::<OrderRested>(&logs)[0].quantity, 3);
                    let _ = take_logs(&mut manual);
                    assert!(world(&mut ctx) == world(&mut manual), "{tag}: STP != full path");
                } else {
                    rejected(&mut ctx, ALICE, &input, E_MARGIN);
                }
                // The boundary itself is where the plan's arithmetic puts it.
                assert_eq!(manual_ok, wallet == need, "{tag} wallet {wallet}");
            }
        }
    }
}

/// T23b (§2.4, Binance's full-size admission): an EXPIRE_TAKER / EXPIRE_BOTH GTC whose owner's
/// order sits at the touch would stop at once and expire (costing nothing), but resting the whole
/// order is unaffordable — it is refused CLEANLY, not turned into an `OrderExpired`; nothing is
/// cancelled.
/// With a wallet that affords the rest it expires with reason 9 as usual.
#[test]
fn t23b_full_size_admission_refuses_an_unaffordable_expire_taker_gtc() {
    for d in DIRS {
        for mode in [ET, EB] {
            let (mut ctx, own) = t23_setup(d, 1, UNIT_VALUE + 1_000_000);
            rejected(&mut ctx, ALICE, &limit(d.taker(), d.lvl(0), 3, GTC, mode), E_MARGIN);
            assert_eq!(level(&mut ctx, d.maker(), d.lvl(0)), (vec![own], 1));

            let (mut ctx, _) = t23_setup(d, 1, WALLET);
            let (out, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(0), 3, GTC, mode));
            assert_eq!(expiry(&logs), (order_id_of(&out), 0, 3, X_STP), "{d:?} {mode:#06b}");
        }
    }
}

/// T21 (§2.4 reduce-only): (a) a reduce-only TAKER under EXPIRE_MAKER is admitted with the same
/// truncated size as ever — STP cancels on the OTHER side cannot change it — and (b) a reduce-only
/// MAKER cancelled by STP is reported with reason 9, not a reduce-only reason.
#[test]
fn t21_reduce_only_and_stp() {
    for d in DIRS {
        // (a) ALICE holds 5 against the taker's side; a reduce-only taker of 8 is cut to 5.
        let mut ctx = fresh();
        seed_pos(&mut ctx, ALICE, -5 * d.sign(), 1);
        seed_pos(&mut ctx, CAROL, 5 * d.sign(), 1);
        let own = rest(&mut ctx, ALICE, d.maker(), d.lvl(0), 1);
        rest(&mut ctx, BOB, d.maker(), d.lvl(1), 10);
        let (_, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(1), 8, GTC, EM | RO));
        assert_eq!(evs::<OrderPlaced>(&logs)[0].quantity, 5, "{d:?}: admission truncation");
        assert_eq!(cancels(&logs), vec![(ALICE, own, C_STP)]);
        assert_eq!(trades(&logs), vec![(ALICE, BOB, d.lvl(1), 5)]);
        assert_eq!(amount(&mut ctx, ALICE), 0);

        // (b) ALICE holds 5 on the taker's side; her resting reduce-only order sits on the maker
        // side.
        let mut ctx = fresh();
        seed_pos(&mut ctx, ALICE, 5 * d.sign(), 1);
        seed_pos(&mut ctx, CAROL, -5 * d.sign(), 1);
        let ro = rest_with(&mut ctx, ALICE, d.maker(), d.lvl(0), 2, GTC, RO);
        rest(&mut ctx, BOB, d.maker(), d.lvl(1), 1);
        let (_, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(1), 1, GTC, EM));
        assert_eq!(cancels(&logs), vec![(ALICE, ro, C_STP)], "{d:?}");
        assert_eq!(trades(&logs), vec![(ALICE, BOB, d.lvl(1), 1)]);
        assert_eq!(amount(&mut ctx, ALICE), 6 * d.sign());
    }
}

/// T22 (§2.4 band): an own order in a level OUTSIDE the fill-time band (mark $100 ± 10%) is never
/// met, so it triggers no STP under any mode: the walk stops at the band and the remainder expires
/// with reason 0, the own order untouched.
#[test]
fn t22_an_own_order_beyond_the_band_triggers_no_stp() {
    for d in DIRS {
        let (outside, limit_px) = if d.taker() == BUY { (11_100, 11_200) } else { (8_900, 8_800) };
        for mode in MODES {
            for ot in [LIMIT, MKT] {
                let mut ctx = fresh();
                rest(&mut ctx, BOB, d.maker(), MARK, 1);
                let own = rest(&mut ctx, ALICE, d.maker(), outside, 1);
                let px = if ot == MKT { 0 } else { limit_px };
                let (out, logs) = ok(&mut ctx, ALICE, &place(d.taker(), px, 3, ot, IOC, mode));
                let tag = format!("{d:?} {mode:#06b} {ot}");
                assert!(cancels(&logs).is_empty(), "{tag}");
                assert_eq!(trades(&logs), vec![(ALICE, BOB, MARK, 1)], "{tag}");
                assert_eq!(expiry(&logs), (order_id_of(&out), 1, 2, X_TIF), "{tag}");
                assert_eq!(level(&mut ctx, d.maker(), outside), (vec![own], 1));
            }
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §4 The per-market open-order cap (Appendix B2)
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// A signed placement refused AFTER its signature burn: the exact reason, no log, the world
/// bit-identical, at least one (burn) write — and the signature is spent.
fn rejected_signed(ctx: &mut TestCtx, input: &[u8], expect: &str, duplicate: &str) {
    let _ = take_logs(ctx);
    let before = world(ctx);
    let w = ctx.perp_write_count();
    let out = run_perp_dex_call(input, GAS, DAVE, U256::ZERO, false, ctx).unwrap();
    assert_eq!(revert_reason(&out), expect);
    assert!(ctx.perp_write_count() > w, "the signature is burned first");
    assert!(take_logs(ctx).is_empty());
    assert!(world(ctx) == before);
    rejected_burning(ctx, DAVE, input, duplicate, 0);
}

/// B2 boundary, both sides counted: at 199 open orders (100 bids + 99 asks) the next order rests;
/// at 200 every order that can REST is refused on either side, for every mode, on the direct and
/// the signed path — judged on the PRE-match count, so even a GTC that would fill completely, or
/// one that would only STP-cancel the owner's own orders, is refused. Others are unaffected, and
/// freeing one slot re-admits exactly one.
#[test]
fn b2_the_200th_open_order_rests_and_the_201st_is_refused_on_either_side() {
    let mut ctx = fresh();
    register_alice_key(&mut ctx);
    let bids = fill_book(&mut ctx, ALICE, BUY, 9_000, 100);
    fill_book(&mut ctx, ALICE, SELL, 11_000, 99);
    assert_eq!(open_count(&mut ctx, ALICE), 199);
    rest(&mut ctx, ALICE, SELL, 11_000, 1);
    assert_eq!(open_count(&mut ctx, ALICE), CAP);

    for (side, price) in [(BUY, 9_000), (SELL, 11_000), (BUY, 9_500), (SELL, 10_500)] {
        for tif in [GTC, PO] {
            for mode in MODES {
                rejected(&mut ctx, ALICE, &limit(side, price, 1, tif, mode), E_CAP);
            }
        }
    }
    rest(&mut ctx, BOB, SELL, 10_000, 1);
    rejected(&mut ctx, ALICE, &limit(BUY, 10_000, 1, GTC, 0), E_CAP);
    rejected(&mut ctx, ALICE, &limit(BUY, 11_000, 1, GTC, EM), E_CAP);
    for (salt, side, price) in [(1u8, BUY, 9_000), (2, SELL, 11_000)] {
        let input = signed_place(&mut ctx, side, price, 1, GTC, 0, salt);
        rejected_signed(&mut ctx, &input, E_CAP, E_DUP_SIG);
    }
    assert_eq!(open_count(&mut ctx, ALICE), CAP);
    assert_eq!(nonce(&mut ctx, ALICE), 200, "a refused placement consumes no id");

    rest(&mut ctx, CAROL, BUY, 9_000, 1);
    ok(&mut ctx, ALICE, &cancel(bids[0]));
    rest(&mut ctx, ALICE, BUY, 9_000, 1);
    rejected(&mut ctx, ALICE, &limit(BUY, 9_000, 1, GTC, 0), E_CAP);
}

/// B2: the cap never refuses an order that cannot rest — IOC, FOK and market orders on both sides
/// still trade at the cap (a user at the cap can always close), and one that crosses nothing still
/// expires as usual. The count is unchanged by them.
#[test]
fn b2_at_the_cap_ioc_fok_and_market_orders_still_trade() {
    let mut ctx = fresh();
    fill_book(&mut ctx, ALICE, BUY, 9_000, 100);
    fill_book(&mut ctx, ALICE, SELL, 11_000, 100);
    rest(&mut ctx, BOB, SELL, 10_000, 10);
    rest(&mut ctx, CAROL, BUY, 9_500, 10);
    for (side, px, maker) in [(BUY, 10_000, BOB), (SELL, 9_500, CAROL)] {
        for input in [limit(side, px, 1, IOC, 0), limit(side, px, 1, FOK, 0), market(side, 1, 0)] {
            let (_, logs) = ok(&mut ctx, ALICE, &input);
            assert_eq!(trades(&logs), vec![(ALICE, maker, px, 1)]);
        }
    }
    let (out, logs) = ok(&mut ctx, ALICE, &limit(BUY, 9_100, 1, IOC, 0));
    assert_eq!(expiry(&logs), (order_id_of(&out), 0, 1, X_TIF));
    assert_eq!(open_count(&mut ctx, ALICE), CAP);
}

/// B2: reduce-only resting orders COUNT toward the cap, and a reduce-only IOC / market order still
/// closes at the cap.
#[test]
fn b2_reduce_only_orders_count_and_a_reduce_only_close_still_trades_at_the_cap() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 300, 1);
    seed_pos(&mut ctx, CAROL, -300, 1);
    fill_book(&mut ctx, ALICE, BUY, 9_000, 150);
    for _ in 0..49 {
        rest_with(&mut ctx, ALICE, SELL, 11_000, 1, GTC, RO);
    }
    assert_eq!(open_count(&mut ctx, ALICE), 199);
    rest_with(&mut ctx, ALICE, SELL, 11_000, 1, GTC, RO);
    assert_eq!(open_count(&mut ctx, ALICE), CAP);
    rejected(&mut ctx, ALICE, &limit(SELL, 11_000, 1, GTC, RO), E_CAP);
    rejected(&mut ctx, ALICE, &limit(SELL, 10_500, 1, PO, RO | ET), E_CAP);
    rest(&mut ctx, BOB, BUY, 9_500, 5);
    let (_, logs) = ok(&mut ctx, ALICE, &limit(SELL, 9_500, 2, IOC, RO));
    assert_eq!(trades(&logs), vec![(ALICE, BOB, 9_500, 2)]);
    let (_, logs) = ok(&mut ctx, ALICE, &market(SELL, 1, RO));
    assert_eq!(trades(&logs), vec![(ALICE, BOB, 9_500, 1)]);
    assert_eq!(amount(&mut ctx, ALICE), 297);
    assert_eq!(open_count(&mut ctx, ALICE), CAP);
}

fn stop_input(qty: u64) -> Vec<u8> {
    algo(SELL, STOP_MARKET, 9_500, 0, qty, 0, RO)
}

/// B2: untriggered conditionals count in the combined cap, and their own 32 sub-cap still binds
/// first. Every refusal — book or conditional, sub-cap or combined — is write-clean and consumes
/// no id; cancelling either kind frees a slot for either kind.
#[test]
fn b2_conditionals_share_the_cap_and_their_32_sub_cap_binds_first() {
    let mut ctx = fresh();
    register_alice_key(&mut ctx);
    seed_pos(&mut ctx, ALICE, 300, 1);
    seed_pos(&mut ctx, CAROL, -300, 1);
    let mut algos = Vec::new();
    for _ in 0..ALGO_CAP {
        let (out, _) = ok(&mut ctx, ALICE, &stop_input(1));
        algos.push(placeAlgoOrderCall::abi_decode_returns(&out.bytes).unwrap().0);
    }
    rejected(&mut ctx, ALICE, &stop_input(1), E_ALGO_SUBCAP);
    let bids = fill_book(&mut ctx, ALICE, BUY, 9_000, 167);
    assert_eq!(open_count(&mut ctx, ALICE), 199);
    rest(&mut ctx, ALICE, BUY, 9_000, 1);
    assert_eq!(open_count(&mut ctx, ALICE), CAP);
    rejected(&mut ctx, ALICE, &limit(BUY, 9_000, 1, GTC, 0), E_CAP);
    rejected(&mut ctx, ALICE, &stop_input(1), E_ALGO_SUBCAP);

    // 168 book + 31 conditionals: a book order fills the 200th slot …
    ok(
        &mut ctx,
        ALICE,
        &cancelAlgoOrderCall {
            algoId: algos[0].into(),
            marketId: MARKET_ID,
        }
        .abi_encode(),
    );
    rest(&mut ctx, ALICE, BUY, 9_000, 1);
    assert_eq!(open_count(&mut ctx, ALICE), CAP);
    // … after which a conditional is refused by the COMBINED cap (31 < 32), directly and signed.
    rejected(&mut ctx, ALICE, &stop_input(1), E_ALGO_CAP);
    let input = signed_algo(&mut ctx, SELL, 9_500, 1, RO);
    rejected_signed(&mut ctx, &input, E_ALGO_CAP, E_DUP_ALGO_SIG);
    // A cancelled book order frees the slot for a conditional.
    ok(&mut ctx, ALICE, &cancel(bids[0]));
    ok(&mut ctx, ALICE, &stop_input(1));
    assert_eq!(open_count(&mut ctx, ALICE), CAP);
    assert_eq!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(), ALGO_CAP);
    rejected(&mut ctx, ALICE, &limit(BUY, 9_000, 1, PO, 0), E_CAP);

    // Book-only 200: a conditional is refused by the combined cap.
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 300, 1);
    seed_pos(&mut ctx, CAROL, -300, 1);
    fill_book(&mut ctx, ALICE, BUY, 9_000, CAP);
    rejected(&mut ctx, ALICE, &stop_input(1), E_ALGO_CAP);
    assert_eq!(nonce(&mut ctx, ALICE), CAP as u64);
}

/// B2 in a batch: each item is checked on the count left by the items before it — the item that
/// takes the 200th slot rests, a later resting item is refused with reason 33 (write-clean, no
/// `OrderPlaced`, no id), an IOC item is unaffected, and the batch goes on.
#[test]
fn b2_a_batch_checks_every_item_against_the_items_before_it() {
    let mut ctx = fresh();
    fill_book(&mut ctx, ALICE, BUY, 9_000, 100);
    fill_book(&mut ctx, ALICE, SELL, 11_000, 99);
    let (out, logs) = ok(
        &mut ctx,
        ALICE,
        &batch(vec![
            item(BUY, 9_000, 1, LIMIT, GTC, 0),
            item(SELL, 11_000, 1, LIMIT, GTC, 0),
            item(BUY, 9_100, 1, LIMIT, IOC, 0),
            item(BUY, 9_000, 1, LIMIT, PO, ET),
        ]),
    );
    assert_eq!(
        batch_records(&out),
        vec![
            (TAG_ACCEPTED, oid(ALICE, 199), 0),
            (TAG_REJECTED, [0u8; 32], B_MAX_OPEN),
            (TAG_TERMINAL, oid(ALICE, 200), 0),
            (TAG_REJECTED, [0u8; 32], B_MAX_OPEN),
        ]
    );
    let placed: Vec<_> = evs::<OrderPlaced>(&logs).iter().map(|e| e.orderId.0).collect();
    assert_eq!(placed, vec![oid(ALICE, 199), oid(ALICE, 200)]);
    assert_eq!(nonce(&mut ctx, ALICE), 201);
    assert_eq!(open_count(&mut ctx, ALICE), CAP);
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §5 Batches (T19, §2.4)
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// T19: items run in order and a later item is the TAKER against earlier ones. Under the default
/// mode `[GTC maker, GTC crossing]` → both Accepted, item 0 cancelled with reason 9 by item 1, item
/// 1 rests in full; the logs, the final state and the nonce are exactly those of the same two
/// orders sent as two transactions (no money moved, so there is no drain row to merge). A FOK item
/// still self-trades; a PostOnly item is refused (27); an IOC item cancels and expires with reason
/// 0; an EXPIRE_TAKER item stops and expires with reason 9, leaving item 0 resting.
#[test]
fn t19_inside_a_batch_a_later_item_is_the_taker_against_earlier_ones() {
    for d in DIRS {
        let (id0, id1) = (oid(ALICE, 0), oid(ALICE, 1));
        let maker = item(d.maker(), d.lvl(0), 2, LIMIT, GTC, 0);

        let mut ctx = fresh();
        let crossing = item(d.taker(), d.lvl(0), 2, LIMIT, GTC, EM);
        let (out, logs) = ok(&mut ctx, ALICE, &batch(vec![maker.clone(), crossing]));
        assert_eq!(batch_records(&out), vec![(TAG_ACCEPTED, id0, 0), (TAG_ACCEPTED, id1, 0)]);
        assert_eq!(names(&logs), vec![OP, OR, OP, OC, OR], "{d:?}");
        assert_eq!(cancels(&logs), vec![(ALICE, id0, C_STP)]);
        let r = &evs::<OrderRested>(&logs)[1];
        assert_eq!((r.orderId.0, r.quantity), (id1, 2));
        let mut twin = fresh();
        let (_, l0) = ok(&mut twin, ALICE, &limit(d.maker(), d.lvl(0), 2, GTC, 0));
        let (_, l1) = ok(&mut twin, ALICE, &limit(d.taker(), d.lvl(0), 2, GTC, EM));
        let separate: Vec<_> = l0.into_iter().chain(l1).collect();
        assert_eq!(logs, separate, "{d:?}: batch stream != two transactions");
        assert!(world(&mut twin) == world(&mut ctx), "{d:?}: batch state != two transactions");
        assert_eq!(nonce(&mut ctx, ALICE), 2);

        for mode in MODES {
            let mut ctx = fresh();
            let fok = item(d.taker(), d.lvl(0), 2, LIMIT, FOK, mode);
            let (out, logs) = ok(&mut ctx, ALICE, &batch(vec![maker.clone(), fok]));
            assert_eq!(batch_records(&out), vec![(TAG_ACCEPTED, id0, 0), (TAG_TERMINAL, id1, 0)]);
            assert_eq!(trades(&logs), vec![(ALICE, ALICE, d.lvl(0), 2)]);
        }

        let mut ctx = fresh();
        let (out, logs) = ok(
            &mut ctx,
            ALICE,
            &batch(vec![
                item(d.maker(), d.lvl(0), 1, LIMIT, GTC, 0),
                item(d.taker(), d.lvl(0), 1, LIMIT, PO, EM),
                item(d.taker(), d.lvl(0), 1, LIMIT, IOC, EM),
            ]),
        );
        assert_eq!(
            batch_records(&out),
            vec![
                (TAG_ACCEPTED, id0, 0),
                (TAG_REJECTED, [0u8; 32], B_POST_ONLY),
                (TAG_TERMINAL, id1, 0),
            ]
        );
        assert_eq!(cancels(&logs), vec![(ALICE, id0, C_STP)]);
        assert_eq!(expiry(&logs), (id1, 0, 1, X_TIF));

        let mut ctx = fresh();
        let (out, logs) = ok(
            &mut ctx,
            ALICE,
            &batch(vec![
                item(d.maker(), d.lvl(0), 1, LIMIT, GTC, 0),
                item(d.taker(), d.lvl(0), 1, LIMIT, GTC, ET),
            ]),
        );
        assert_eq!(batch_records(&out), vec![(TAG_ACCEPTED, id0, 0), (TAG_TERMINAL, id1, 0)]);
        assert!(cancels(&logs).is_empty());
        assert_eq!(expiry(&logs), (id1, 0, 1, X_STP));
        assert_eq!(level(&mut ctx, d.maker(), d.lvl(0)), (vec![id0], 1));
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §6 Families (T20, D1)
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// `inner` sent by `who`: directly, or by ALICE (the master) through `asSubAccount`.
fn act(ctx: &mut TestCtx, who: Address, inner: Vec<u8>) -> Vec<primitives::Log> {
    if who == ALICE {
        ok(ctx, ALICE, &inner).1
    } else {
        let wrapped = asSubAccountCall { subAccount: who, data: inner.into() }.abi_encode();
        ok(ctx, ALICE, &wrapped).1
    }
}

/// T20 (D1): STP compares ADDRESSES only. A master and its sub-account, or two siblings, are
/// different addresses and trade with each other as usual — under every mode, both sides.
#[test]
fn t20_a_master_and_its_sub_accounts_still_trade_with_each_other() {
    for d in DIRS {
        let mut ctx = fresh();
        let mut subs = Vec::new();
        for name in ["s0", "s1"] {
            let create = createSubAccountCall { name: name.into() }.abi_encode();
            let (out, _) = ok(&mut ctx, ALICE, &create);
            let sub = createSubAccountCall::abi_decode_returns(&out.bytes).unwrap();
            ok(
                &mut ctx,
                ALICE,
                &internalTransferCall {
                    to: sub,
                    fromWallet: 1,
                    toWallet: 1,
                    amount: 1_000_000_000_000,
                }
                .abi_encode(),
            );
            subs.push(sub);
        }
        let (s0, s1) = (subs[0], subs[1]);
        for (maker, taker) in [(ALICE, s0), (s0, ALICE), (s1, s0), (s0, s1)] {
            for mode in MODES {
                act(&mut ctx, maker, limit(d.maker(), d.lvl(0), 1, GTC, 0));
                let logs = act(&mut ctx, taker, limit(d.taker(), d.lvl(0), 1, GTC, mode));
                assert_eq!(trades(&logs), vec![(taker, maker, d.lvl(0), 1)], "{d:?} {mode:#06b}");
                assert!(cancels(&logs).is_empty());
            }
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §7 Silence (T16, D9): a cancel-only participant publishes no account row
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// T16 / D9 (Binance: cancelled orders push no ACCOUNT_UPDATE): a taker whose only liquidity is its
/// owner's order — so nothing fills — publishes exactly the order events, for every mode and every
/// STP kind: no `AccountBalanceChanged`, no `PositionChanged`.
#[test]
fn t16_a_zero_fill_stp_publishes_no_account_row() {
    for d in DIRS {
        for mode in MODES {
            for kind in STP_KINDS {
                let mut ctx = fresh();
                book(&mut ctx, d, &[&[(ALICE, 1)]]);
                let (out, logs) = ok(&mut ctx, ALICE, &taker(d, kind, 0, 1, mode));
                let tag = format!("{d:?} {mode:#06b} {kind:?}");
                let rests = kind == (LIMIT, GTC) && mode == EM;
                let want = match (mode, rests) {
                    (EM, true) => vec![OP, OC, OR],
                    (EM, false) | (EB, _) => vec![OP, OC, OE],
                    _ => vec![OP, OE],
                };
                assert_eq!(names(&logs), want, "{tag}");
                if !rests {
                    let reason = if mode == EM { X_TIF } else { X_STP };
                    assert_eq!(expiry(&logs), (order_id_of(&out), 0, 1, reason), "{tag}");
                }
            }
        }
    }
}

/// T16 / D9 with funding owed: a zero-fill STP taker is pulled into the match by its own cancelled
/// order, which settles its funding (§2.4) — so the ONLY account row is the funding group's
/// (reason FUNDING_FEE), placed before the cancel, and no order row follows it.
#[test]
fn t16_with_funding_owed_the_funding_group_is_the_only_account_row() {
    for d in DIRS {
        let mut ctx = fresh();
        rest(&mut ctx, BOB, d.maker(), d.lvl(0), 1);
        ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(0), 1, IOC, 0));
        assert_eq!(amount(&mut ctx, ALICE), d.sign());
        let own = rest(&mut ctx, ALICE, d.maker(), d.lvl(1), 1);
        let mut f = storage::load_funding_state(&mut ctx, MARKET_ID).unwrap();
        f.cumulative_funding_index += 100_000_000; // $100 × 1%
        storage::save_funding_state(&mut ctx, MARKET_ID, &f).unwrap();

        let (_, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.lvl(1), 1, GTC, EM));
        let fs = evs::<FundingSettled>(&logs);
        assert_eq!(fs.len(), 1, "{d:?}: {:?}", names(&logs));
        // −(amount × 1e8 × 1e6) / (1e2 × 1e0 × 1e6)
        assert_eq!((fs[0].user, fs[0].amount), (ALICE, -d.sign() * 1_000_000));
        let rows: Vec<_> = evs::<AccountBalanceChanged>(&logs)
            .iter()
            .map(|e| (e.user, e.reason))
            .collect();
        assert_eq!(rows, vec![(ALICE, AccountUpdateReason::FundingFee as u8)], "{d:?}");
        assert_eq!(cancels(&logs), vec![(ALICE, own, C_STP)]);
        let order_events: Vec<_> = names(&logs)
            .into_iter()
            .filter(|n| [OP, OC, OR, OE, TR].contains(n))
            .collect();
        assert_eq!(order_events, vec![OP, OC, OR]);
        let n = names(&logs);
        let at = |x: &str| n.iter().position(|y| *y == x).unwrap();
        for g in [ABC, PC, "FundingSettled"] {
            assert!(
                at(OP) < at(g) && at(g) < at(OC),
                "{d:?}: the funding group sits before the cancel: {n:?}"
            );
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §8 Conditionals: inheritance (T15, D5), trigger rejects (D11 / T25), the trigger at the cap (B2)
// ═════════════════════════════════════════════════════════════════════════════════════════════
//
// `d` names the FIRED order's side. ALICE holds 10 against it (so the conditional is reduce-only)
// and CAROL the opposite, seeded off-book: `last_traded` stays 0, so the oracle tick sets
// `mark == index` exactly. A STOP SELL fires at mark <= 9_500 (tick to 9_400, band 8_460..10_340);
// a STOP BUY at mark >= 10_500 (tick to 10_600, band 9_540..11_660).

fn fire_fixture(d: Dir) -> TestCtx {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, -10 * d.sign(), 1);
    seed_pos(&mut ctx, CAROL, 10 * d.sign(), 1);
    ctx
}

fn trigger_px(d: Dir) -> u64 {
    if d.taker() == BUY {
        10_500
    } else {
        9_500
    }
}

fn tick_px(d: Dir) -> u64 {
    if d.taker() == BUY {
        10_600
    } else {
        9_400
    }
}

/// Maker level `k` for the fired order, inside the post-tick band.
fn flvl(d: Dir, k: u64) -> u64 {
    if d.taker() == BUY {
        10_700 + 100 * k
    } else {
        9_300 - 100 * k
    }
}

/// A limit price past the post-tick band on the fired order's side: resting there would make it a
/// best quote "too good to be true", which the rest refuses.
fn too_good(d: Dir) -> u64 {
    if d.taker() == BUY {
        12_000
    } else {
        8_000
    }
}

fn algo_id_of(out: &PerpOutput) -> [u8; 32] {
    placeAlgoOrderCall::abi_decode_returns(&out.bytes).unwrap().0
}

/// ALICE's part of a log stream, as event names.
fn alice_names(logs: &[primitives::Log]) -> Vec<&'static str> {
    seq(logs).into_iter().filter(|&(_, u)| u == ALICE).map(|(n, _)| n).collect()
}

/// T15 (D5, + M15, M25): a reduce-only STOP_MARKET placed with a mode walks, once fired, under
/// THAT mode — its `OrderPlaced` echoes it — against a path holding its owner's own order: default
/// cancels it and fills deeper; EXPIRE_TAKER stops and expires (reason 9) with the own order kept;
/// EXPIRE_BOTH cancels it and expires. A `closePosition` conditional inherits it the same way.
#[test]
fn t15_a_fired_conditional_walks_under_the_mode_it_was_placed_with() {
    for d in DIRS {
        for mode in MODES {
            let tag = format!("{d:?} {mode:#06b}");
            let mut ctx = fire_fixture(d);
            rest(&mut ctx, BOB, d.maker(), flvl(d, 0), 1);
            let own = rest(&mut ctx, ALICE, d.maker(), flvl(d, 1), 2);
            rest(&mut ctx, DAVE, d.maker(), flvl(d, 2), 5);
            let stop = algo(d.taker(), STOP_MARKET, trigger_px(d), 0, 3, 0, RO | mode);
            let (out, _) = ok(&mut ctx, ALICE, &stop);
            let algo_id = algo_id_of(&out);
            let logs = tick(&mut ctx, tick_px(d));
            let trig = evs::<AlgoOrderTriggered>(&logs);
            assert_eq!(trig.len(), 1, "{tag}");
            assert_eq!(trig[0].algoId.0, algo_id);
            let placed = evs::<OrderPlaced>(&logs);
            assert_eq!(placed.len(), 1, "{tag}");
            assert_eq!(
                (placed[0].user, placed[0].quantity, placed[0].flags),
                (ALICE, 3, RO | mode),
                "{tag}"
            );
            let fired = placed[0].orderId.0;
            match mode {
                EM => {
                    assert_eq!(
                        trades(&logs),
                        vec![(ALICE, BOB, flvl(d, 0), 1), (ALICE, DAVE, flvl(d, 2), 2)],
                        "{tag}"
                    );
                    assert_eq!(cancels(&logs), vec![(ALICE, own, C_STP)], "{tag}");
                    assert!(evs::<OrderExpired>(&logs).is_empty(), "{tag}");
                    assert_eq!(amount(&mut ctx, ALICE), -7 * d.sign());
                }
                ET => {
                    assert_eq!(trades(&logs), vec![(ALICE, BOB, flvl(d, 0), 1)], "{tag}");
                    assert!(cancels(&logs).is_empty(), "{tag}");
                    assert_eq!(expiry(&logs), (fired, 1, 2, X_STP), "{tag}");
                    assert_eq!(level(&mut ctx, d.maker(), flvl(d, 1)), (vec![own], 1));
                    assert_eq!(amount(&mut ctx, ALICE), -9 * d.sign());
                }
                _ => {
                    assert_eq!(trades(&logs), vec![(ALICE, BOB, flvl(d, 0), 1)], "{tag}");
                    assert_eq!(cancels(&logs), vec![(ALICE, own, C_STP)], "{tag}");
                    assert_eq!(expiry(&logs), (fired, 1, 2, X_STP), "{tag}");
                    assert_eq!(amount(&mut ctx, ALICE), -9 * d.sign());
                }
            }
            assert_eq!(amount(&mut ctx, DAVE) != 0, mode == EM, "{tag}: DAVE reached iff default");
            assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
        }

        // closePosition + EXPIRE_TAKER: the whole |position| (10) fires, stops at the own order.
        let mut ctx = fire_fixture(d);
        rest(&mut ctx, BOB, d.maker(), flvl(d, 0), 1);
        let own = rest(&mut ctx, ALICE, d.maker(), flvl(d, 1), 2);
        ok(
            &mut ctx,
            ALICE,
            &algo(d.taker(), STOP_MARKET, trigger_px(d), 0, 0, 0, ALGO_FLAG_CLOSE_POSITION | ET),
        );
        let logs = tick(&mut ctx, tick_px(d));
        let placed = evs::<OrderPlaced>(&logs);
        assert_eq!((placed[0].quantity, placed[0].flags), (10, RO | ET), "{d:?} closePosition");
        assert_eq!(expiry(&logs), (placed[0].orderId.0, 1, 9, X_STP));
        assert_eq!(level(&mut ctx, d.maker(), flvl(d, 1)), (vec![own], 1));
    }
}

/// The D11 fixture: `fire_fixture` plus, when `with_liquidation`, DAVE holding 1 on ALICE's side
/// at 5x ($20 margin) — under maintenance at the tick's mark (equity $14 < $94/6 or $106/6) — with
/// CAROL holding the opposite 11, so the SAME tick must liquidate DAVE after serving the trigger.
fn d11_fixture(d: Dir, with_liquidation: bool) -> TestCtx {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, -10 * d.sign(), 1);
    if with_liquidation {
        seed_pos(&mut ctx, DAVE, -d.sign(), 5);
        seed_pos(&mut ctx, CAROL, 11 * d.sign(), 1);
    } else {
        seed_pos(&mut ctx, CAROL, 10 * d.sign(), 1);
    }
    ctx
}

/// The D11 outcome: the oracle tick SUCCEEDED (`tick` asserts it), ALICE's part of the stream is
/// exactly `AlgoOrderTriggered · AlgoOrderCancelled(10)`, the conditional is gone from every index,
/// her position is untouched, and — when the fixture has one — the liquidation that follows the
/// trigger in the same tick still ran.
fn assert_trigger_rejected(
    ctx: &mut TestCtx,
    logs: &[primitives::Log],
    d: Dir,
    algo_id: [u8; 32],
    liq: bool,
    tag: &str,
) {
    assert_eq!(alice_names(logs), vec!["AlgoOrderTriggered", "AlgoOrderCancelled"], "{tag}");
    let c = evs::<AlgoOrderCancelled>(logs);
    assert_eq!((c[0].algoId.0, c[0].reason), (algo_id, C_TRIGGER_REJECTED), "{tag}");
    assert!(evs::<OrderPlaced>(logs).iter().all(|e| e.user != ALICE), "{tag}");
    assert_eq!(order(ctx, algo_id), None, "{tag}: the record is deleted");
    assert!(storage::load_user_algo_orders(ctx, ALICE, MARKET_ID).unwrap().is_empty());
    assert_eq!(amount(ctx, ALICE), -10 * d.sign(), "{tag}");
    if liq {
        let n = seq(logs);
        let cancelled = n.iter().position(|&(e, _)| e == "AlgoOrderCancelled").unwrap();
        let liquidated = n.iter().position(|&(e, u)| e == "Liquidation" && u == DAVE);
        assert!(
            liquidated.is_some_and(|l| l > cancelled),
            "{tag}: the liquidation after the trigger did not run: {n:?}"
        );
        assert_eq!(amount(ctx, DAVE), 0, "{tag}");
    }
}

/// D11 / T25 (+ M29): a fired conditional whose placement the matcher REFUSES — an unfillable FOK,
/// a PostOnly that would cross (a third party's order or its owner's own), a GTC whose rest is
/// refused as a too-good best (under every mode) — is cancelled with reason 10 inside a SUCCESSFUL
/// oracle transaction, and the liquidation the same tick owes still runs.
#[test]
fn d11_a_trigger_the_matcher_refuses_is_cancelled_and_the_tick_goes_on() {
    for d in DIRS {
        let cases: Vec<(&str, u64, u8, u64, u8, bool)> = vec![
            // (case, price, tif, qty, flags, BOB at flvl(0))
            ("FOK unfillable", flvl(d, 0), FOK, 5, RO, true),
            ("PostOnly crossing", flvl(d, 0), PO, 1, RO, true),
            ("too-good GTC, EXPIRE_MAKER", too_good(d), GTC, 5, RO | EM, false),
            ("too-good GTC, EXPIRE_TAKER", too_good(d), GTC, 5, RO | ET, false),
            ("too-good GTC, EXPIRE_BOTH", too_good(d), GTC, 5, RO | EB, false),
        ];
        for (case, price, tif, qty, flags, bob) in cases {
            let tag = format!("{d:?} {case}");
            let mut ctx = d11_fixture(d, true);
            if bob {
                rest(&mut ctx, BOB, d.maker(), flvl(d, 0), 1);
            }
            let stop = algo(d.taker(), STOP, trigger_px(d), price, qty, tif, flags);
            let (out, _) = ok(&mut ctx, ALICE, &stop);
            let algo_id = algo_id_of(&out);
            let logs = tick(&mut ctx, tick_px(d));
            assert_trigger_rejected(&mut ctx, &logs, d, algo_id, true, &tag);
        }

        // A PostOnly that would cross only its OWNER's order is refused the same way (D3); the own
        // order stays.
        let mut ctx = d11_fixture(d, false);
        let own = rest(&mut ctx, ALICE, d.maker(), flvl(d, 0), 1);
        let stop = algo(d.taker(), STOP, trigger_px(d), flvl(d, 0), 1, PO, RO | EB);
        let (out, _) = ok(&mut ctx, ALICE, &stop);
        let algo_id = algo_id_of(&out);
        let logs = tick(&mut ctx, tick_px(d));
        let tag = format!("{d:?} PostOnly vs own");
        assert_trigger_rejected(&mut ctx, &logs, d, algo_id, false, &tag);
        assert_eq!(level(&mut ctx, d.maker(), flvl(d, 0)), (vec![own], 1));
    }
}

/// D11 × §2.3 with the owner's order on the path of a too-good GTC: EXPIRE_MAKER cancels it in the
/// walk, the rest is then refused, and the WHOLE placement is undone (reason 10, own order intact).
/// EXPIRE_TAKER / EXPIRE_BOTH stop at it, so the remainder EXPIRES (reason 9) and never reaches the
/// refused rest: the fired order is accepted, no reason 10.
#[test]
fn d11_with_the_owners_order_on_the_path_only_expire_maker_reaches_the_refused_rest() {
    for d in DIRS {
        for mode in MODES {
            let tag = format!("{d:?} {mode:#06b}");
            let mut ctx = d11_fixture(d, false);
            let own = rest(&mut ctx, ALICE, d.maker(), flvl(d, 0), 1);
            let stop = algo(d.taker(), STOP, trigger_px(d), too_good(d), 5, GTC, RO | mode);
            let (out, _) = ok(&mut ctx, ALICE, &stop);
            let algo_id = algo_id_of(&out);
            let logs = tick(&mut ctx, tick_px(d));
            match mode {
                EM => {
                    assert_trigger_rejected(&mut ctx, &logs, d, algo_id, false, &tag);
                    assert_eq!(level(&mut ctx, d.maker(), flvl(d, 0)), (vec![own], 1), "{tag}");
                }
                ET => {
                    assert_eq!(alice_names(&logs), vec!["AlgoOrderTriggered", OP, OE], "{tag}");
                    let fired = evs::<OrderPlaced>(&logs)[0].orderId.0;
                    assert_eq!(expiry(&logs), (fired, 0, 5, X_STP), "{tag}");
                    assert_eq!(level(&mut ctx, d.maker(), flvl(d, 0)), (vec![own], 1), "{tag}");
                }
                _ => {
                    assert_eq!(alice_names(&logs), vec!["AlgoOrderTriggered", OP, OC, OE], "{tag}");
                    assert_eq!(cancels(&logs), vec![(ALICE, own, C_STP)], "{tag}");
                    let fired = evs::<OrderPlaced>(&logs)[0].orderId.0;
                    assert_eq!(expiry(&logs), (fired, 0, 5, X_STP), "{tag}");
                }
            }
            let algo_cancels = evs::<AlgoOrderCancelled>(&logs);
            assert!(algo_cancels.iter().all(|e| e.reason == C_TRIGGER_REJECTED));
            assert_eq!(amount(&mut ctx, ALICE), -10 * d.sign(), "{tag}");
        }
    }
}

/// B2: a trigger is never refused by the cap. ALICE at exactly 200 open orders (199 resting + 1
/// conditional that rests when it fires): the tick fires it, it RESTS (no reason 10), the count
/// stays 200 (conditional −1, book +1), and the cap still refuses the next submission.
#[test]
fn b2_a_trigger_at_the_cap_still_rests() {
    for d in DIRS {
        let mut ctx = fire_fixture(d);
        let far = if d.taker() == BUY { 11_000 } else { 9_000 };
        fill_book(&mut ctx, ALICE, d.maker(), far, CAP - 1);
        let rest_px = if d.taker() == BUY { 10_200 } else { 9_800 };
        ok(&mut ctx, ALICE, &algo(d.taker(), STOP, trigger_px(d), rest_px, 1, GTC, RO));
        assert_eq!(open_count(&mut ctx, ALICE), CAP);
        rejected(&mut ctx, ALICE, &limit(d.maker(), far, 1, GTC, 0), E_CAP);
        let stop = algo(d.taker(), STOP_MARKET, trigger_px(d), 0, 1, 0, RO);
        rejected(&mut ctx, ALICE, &stop, E_ALGO_CAP);

        let logs = tick(&mut ctx, tick_px(d));
        assert_eq!(alice_names(&logs), vec!["AlgoOrderTriggered", OP, OR], "{d:?}");
        assert_eq!(evs::<OrderRested>(&logs)[0].price, rest_px);
        assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
        assert_eq!(open_ids(&mut ctx, ALICE, d.taker()).len(), 1);
        assert_eq!(open_count(&mut ctx, ALICE), CAP);
        rejected(&mut ctx, ALICE, &limit(d.maker(), far, 1, GTC, 0), E_CAP);
    }
}
