//! **Sub-accounts (P1), written from the spec (`SPEC_subaccounts_P1.md` §9) by an independent
//! writer — NOT from the implementation's own tests** (`sub_account_tests.rs`).
//!
//! Every call goes through the full shell (`run_perp_dex_call`), so the gas charge, the static
//! check, the EOA depth guard, the commit-only #23 tripwire (a `debug_assert` on any
//! write-then-revert) and the end-of-call `AccountBalanceChanged` drain are all in the loop. Every
//! rejection is asserted four ways (see [`rejected`]): the exact reason string, `perp_write_count`
//! did not move, the call logged nothing, and every watched account / position / key list / market
//! index / algo list plus the book's price lists is BIT-identical (encoded blobs, not field reads).
//!
//! The addresses and direct-path order ids the engine DERIVES are pinned as literals computed
//! OUTSIDE this codebase with ethers v6 —
//! `getAddress(dataSlice(keccak256(concat([toUtf8Bytes("perpdex_subaccount_v1"), master,
//! Uint8Array([i])])), 12))` and `keccak256(concat([owner, zeroPadValue(toBeHex(nonce), 8)]))` —
//! so no assertion here can pass merely because the test and the engine call the same helper.
//!
//! Self-contained: the market fixture below (`setup_market` and the few helpers around it) is a
//! COPY of the risk suite's (`risk::tests`), not a reach into it, so a change to that fixture
//! cannot silently move the numbers this suite pins.

use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use ed25519_dalek::{Signer, SigningKey};
use primitives::{address, b256, hardfork::SpecId, keccak256, Address, FixedBytes, U256};
use std::collections::BTreeSet;

use crate::{
    run_perp_dex_call, storage,
    trading::run_place_order,
    types::{AccountUpdateReason, MarginTiers, Market, PerpPosition, Side, UserAccount},
    PerpError, PERP_DEX_ADDRESS, USDC_ADDRESS,
};
use crate::call::{selectors_map, PerpOutput};
use crate::events::stream_test_support::{account_update_reasons, assert_account_update_groups};
use crate::host::PerpHost;
use crate::interface::IPerpDex::{
    addMarketCall, addPositionMarginCall, asSubAccountCall, batchCancelOrdersCall,
    batchCancelOrdersSignedCall, batchPlaceOrdersCall, batchPlaceOrdersSignedCall,
    cancelAlgoOrderCall, cancelAlgoOrderSignedCall, cancelOrderCall, cancelOrderSignedCall,
    createSubAccountCall, depositCall, depositInsuranceFundCall, drainDeferredWorkCall,
    getAccountCall, getAccountMarginCall, getAdminCall, getAlgoOrderCall, getApiKeyCall,
    getApiKeysCall, getAveragePremiumIndexCall, getBookLevelCall, getBookPricesCall,
    getFundingStateCall, getIndexModeCall, getIndexPriceCall, getInsuranceFundCall,
    getMarginTiersCall, getMarkPriceCall, getMarketCall, getMarketFeeTotalCall,
    getMarketManagerAddressCall, getMasterCall, getOpenAlgoOrdersCall, getOpenOrdersCall,
    getOracleAddressCall, getOrderCall, getPositionCall, getPositionRiskCall,
    getMaxMakerRebateCall, getSubAccountsCall, getSymbolConfigCall, getUserFeeRatesCall,
    initAdminCall, setMaxMakerRebateCall,
    internalTransferCall, liquidateCall, placeAlgoOrderCall, placeAlgoOrderSignedCall,
    placeOrderCall, placeOrderSignedCall, registerApiKeyCall, removePositionMarginCall,
    revokeApiKeyCall, setBasisModeCall, setLeverageCall, setLeverageSignedCall, setMarginTypeCall,
    setMarginTypeSignedCall,
    setMarginTiersCall, setMarketManagerAddressCall, setOracleAddressCall,
    setUserFeeRatesCall, transferAdminCall, transferFromPerpCall, transferFromPerpSignedCall,
    transferToPerpCall, transferToPerpSignedCall, updateIndexPriceCall,
    updateIndexPriceFromBookCall, updateMarketCall, withdrawCall, withdrawInsuranceFundCall,
    AccountBalanceChanged, ApiKeyRegistered, InternalTransfer, Liquidation, OrderPlaced,
    PlaceItem, PositionChanged, SubAccountCreated, Trade, TransferToPerp, UserFeeRatesUpdated,
};
use crate::storage::keys::erc20_balance_slot;
use crate::types::{
    derive_sub_account, AccountUpdateReason as R, AlgoOrderType, ApiKey, MAX_SUB_ACCOUNTS,
};

// ── Market fixture (copied from `risk::tests`) ────────────────────────────────────────────────

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const KEEPER: Address = address!("2222222222222222222222222222222222222222");
const MAKER: Address = address!("3333333333333333333333333333333333333333");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const WHALE: Address = address!("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
const MARKET_ID: u64 = 1;
const PRICE_DECIMALS: u32 = 2;
const ENTRY_PRICE: u64 = 10_000; // $100.00
const USER_WALLET: u64 = 50_000_000; // $50.00 in quote units.
const MAKER_WALLET: u64 = 2_000_000_000; // $2,000.00 in quote units.

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, KEEPER, MAKER, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

/// Market 1 at a $100 mark (base 0 / price 2 decimals, no fees, no funding, no band), plus
/// ALICE's $50 and MAKER's $2,000 perp wallets.
fn setup_market(ctx: &mut TestCtx) {
    storage::save_admin(ctx, ADMIN).unwrap();
    storage::save_market(
        ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 0,
            price_decimals: PRICE_DECIMALS,
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
    storage::save_mark_price(ctx, MARKET_ID, ENTRY_PRICE).unwrap();
    storage::save_account(
        ctx,
        ALICE,
        UserAccount {
            perp_wallet_balance: USER_WALLET as i64,
            ..UserAccount::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    storage::save_account(
        ctx,
        MAKER,
        UserAccount {
            perp_wallet_balance: MAKER_WALLET as i64,
            ..UserAccount::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

/// `E` decoded out of `logs`, in stream order.
fn decode_events<E: SolEvent>(logs: &[primitives::Log]) -> Vec<E> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect()
}

/// A GTC limit order by MAKER, straight into the handler (not through the shell).
fn place_maker_order(ctx: &mut TestCtx, side: u8, price: u64, qty: u64) {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode();
    run_place_order(&input, MAKER, ctx).unwrap();
}

fn position(ctx: &mut TestCtx, user: Address) -> PerpPosition {
    storage::load_position(ctx, user, MARKET_ID).unwrap()
}

/// The DERIVED open-order requirement for `user` in the test market.
fn oo_im(ctx: &mut TestCtx, user: Address) -> u64 {
    let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    let pos = storage::load_position(ctx, user, MARKET_ID).unwrap();
    crate::margin_view::position_open_order_margin(&market, &pos).unwrap()
}

// ── Actors ───────────────────────────────────────────────────────────────────────────────

/// Master A — the family most tests act on.
const MA: Address = address!("00000000000000000000000000000000000000a1");
/// Master B — a SECOND family, for every cross-family attack.
const MB: Address = address!("00000000000000000000000000000000000000b1");
/// An ordinary account with no family — the EXTERNAL counterparty.
const X: Address = address!("00000000000000000000000000000000000000c1");
// `derive(MA, i)` / `derive(MB, 0)` — ethers v6, outside this codebase (see the module docs).
const SA0: Address = address!("4be296967AA4A8a01d731166B60a250ab16d4C65");
const SA1: Address = address!("72e6Dbe7Db384E756E567593f84219Fe5B9f8955");
const SA2: Address = address!("04b09D449F690f90Dddf6839260e28e533031Be8");
const SA9: Address = address!("a388e53cBa8547376370b60B9E3a8D1794C69314");
/// `derive(MA, 10)` — the index the cap forbids. It must never acquire state.
const SA10: Address = address!("66CCDa4cE96634f6A6136808B6DCb5AE7B738BA6");
const SB0: Address = address!("6eb259893a73501AA1d4D81e7365dBb51C403D96");

// Direct-path order ids `keccak256(owner ‖ nonce_be8)` — ethers v6.
const OID_MA_0: [u8; 32] =
    b256!("213ec6c52e69dd0252b9c1cae58e9df8b2949c72d1135adf1d21a06724aa1e07").0;
const OID_SA0_0: [u8; 32] =
    b256!("2ff7d8d818ee7e55b3480fb6e5ca88a8539a5f059974576c332d30d120e6a19a").0;
const OID_SA1_0: [u8; 32] =
    b256!("3bda3c2a89286d626996256f91629c26d6dc303ac2794ec4f8726a78ec01905f").0;
const OID_SA0_1: [u8; 32] =
    b256!("4d9ed97d8c089ca68de03fed5713d2dd6a8cb45baa8135242b25f504033f6328").0;
const OID_SA0_2: [u8; 32] =
    b256!("90a3705d4292e79728a9e1bb49de6dc4228e9853dd79a88423799b1215cc79c5").0;
const OID_SA0_3: [u8; 32] =
    b256!("c8a0af8089e1ced6b32e2e7c60b81caaa4dee3839c73e0ae531ad7d789f12441").0;

// ── Spec constants ───────────────────────────────────────────────────────────────────────

const SPOT: u8 = 0;
const PERP: u8 = 1;
const BUY: u8 = Side::Buy as u8;
const SELL: u8 = Side::Sell as u8;
/// Spec §5: 1 USDC in quote units (6 dp).
const ONE_USDC: u64 = 1_000_000;
/// The wrapper's fixed overhead (spec §3 "a small fixed overhead"; the implementation's value).
const OVERHEAD: u64 = 5_000;
const GAS: u64 = 10_000_000;
const SIGNED_TS: u64 = 1; // == the test block's timestamp
const SIGNED_RECV: u64 = 60;

// ── Exact reject reasons ─────────────────────────────────────────────────────────────────

const E_NOT_MASTER: &str = "asSubAccount: caller is not the master of subAccount";
const E_DENIED: &str = "asSubAccount: inner selector is not allowed for a sub-account";
const E_DENIED_DW: &str =
    "asSubAccount: a sub-account cannot deposit or withdraw (move funds through its master)";
const E_NO_SELECTOR: &str = "asSubAccount: inner call has no selector";
const E_EOA: &str = "perpdex: EOA direct calls only";
const E_CREATE_BY_SUB: &str = "createSubAccount: a sub-account cannot create sub-accounts";
const E_CREATE_CAP: &str = "createSubAccount: sub-account limit reached (max 10)";
const E_CREATE_CLAIMED: &str =
    "createSubAccount: derived sub-account address already has engine state";
const E_NAME_EMPTY: &str = "createSubAccount: name must not be empty";
const E_NAME_LONG: &str = "createSubAccount: name exceeds 32 bytes";
const E_FEE_SUB: &str = "setUserFeeRates: user is a sub-account; set its master's rates";
const E_XFER_WALLET: &str = "internalTransfer: unknown wallet (0 = SPOT, 1 = PERP)";
const E_XFER_ZERO_AMOUNT: &str = "internalTransfer: amount must be > 0";
const E_XFER_TO_ZERO: &str = "internalTransfer: recipient cannot be the zero address";
const E_XFER_TO_DEX: &str = "internalTransfer: recipient cannot be the PerpDEX itself";
const E_XFER_SAME: &str = "internalTransfer: source and destination are the same wallet";
const E_XFER_FROM_SUB: &str =
    "internalTransfer: a sub-account cannot transfer outside its family";
const E_XFER_TO_FOREIGN_SUB: &str =
    "internalTransfer: recipient is a sub-account of another family";
const E_XFER_EXT_WALLETS: &str = "internalTransfer: an external transfer must be SPOT to SPOT";
const E_XFER_EXT_MIN: &str = "internalTransfer: external transfer below the 1 USDC minimum";
const E_XFER_SPOT: &str = "internalTransfer: insufficient spot balance";
const E_XFER_PERP: &str = "internalTransfer: insufficient perp wallet balance";
const E_FROM_PERP: &str = "transferFromPerp: insufficient perp wallet balance";
const E_SIG_BAD: &str = "placeOrderSigned: signature verification failed";
const E_SIG_NO_KEY: &str = "placeOrderSigned: no api key registered for account";
const E_NOT_OWNER: &str = "cancelOrder: not owner";

// ── Shell helpers ────────────────────────────────────────────────────────────────────────

/// The standard market (`setup_market`: $100 mark, base 0 / price 2, no fees, no funding,
/// no band), its incidental logs discarded.
fn ctx() -> TestCtx {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    ctx
}

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

/// One SUCCESSFUL shell call at `gas_limit`; returns the output and exactly this call's logs.
fn ok_at(
    ctx: &mut TestCtx,
    caller: Address,
    input: &[u8],
    gas_limit: u64,
) -> (PerpOutput, Vec<primitives::Log>) {
    let _ = take_logs(ctx);
    let out = run_perp_dex_call(input, gas_limit, caller, U256::ZERO, false, ctx).unwrap();
    assert!(!out.reverted, "unexpected revert: {}", revert_reason(&out));
    let logs = take_logs(ctx);
    (out, logs)
}

fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> (PerpOutput, Vec<primitives::Log>) {
    ok_at(ctx, caller, input, GAS)
}

/// A static view call through the shell.
fn view(ctx: &mut TestCtx, input: &[u8]) -> PerpOutput {
    let out = run_perp_dex_call(input, GAS, X, U256::ZERO, true, ctx).unwrap();
    assert!(!out.reverted, "view reverted: {}", revert_reason(&out));
    out
}

/// Everything one address owns, ENCODED — "unchanged" means bit-identical blobs.
#[derive(Debug, PartialEq)]
struct Owned {
    user: Address,
    account: Vec<u8>,
    position: Vec<u8>,
    api_key_ids: Vec<u8>,
    markets: Vec<u64>,
    algo_orders: Vec<[u8; 32]>,
}

#[derive(Debug, PartialEq)]
struct World {
    owned: Vec<Owned>,
    bids: Vec<u64>,
    asks: Vec<u64>,
    insurance_fund: u64,
}

fn owned(ctx: &mut TestCtx, user: Address) -> Owned {
    Owned {
        user,
        account: perp_core::codec::encode(&storage::load_account(ctx, user).unwrap()).unwrap(),
        position: perp_core::codec::encode(
            &storage::load_position(ctx, user, MARKET_ID).unwrap(),
        )
        .unwrap(),
        api_key_ids: storage::load_api_key_ids(ctx, user).unwrap(),
        markets: storage::load_user_markets(ctx, user).unwrap(),
        algo_orders: storage::load_user_algo_orders(ctx, user, MARKET_ID).unwrap(),
    }
}

/// Every actor any test here touches, plus ALL of A's derivable addresses (incl. index 10).
fn watched() -> Vec<Address> {
    let mut v = vec![MA, MB, X, SB0, ALICE, MAKER, ADMIN, KEEPER, WHALE];
    v.extend((0..=MAX_SUB_ACCOUNTS).map(|i| derive_sub_account(MA, i)));
    v
}

fn world(ctx: &mut TestCtx) -> World {
    let owned = watched().into_iter().map(|u| owned(ctx, u)).collect();
    World {
        owned,
        bids: storage::load_bid_prices(ctx, MARKET_ID).unwrap(),
        asks: storage::load_ask_prices(ctx, MARKET_ID).unwrap(),
        insurance_fund: storage::load_insurance_fund(ctx).unwrap(),
    }
}

/// A REJECTED shell call: exact reason, and nothing written — `perp_write_count` unchanged,
/// no log, every watched blob bit-identical. Returns the output (for its gas).
fn rejected(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) -> PerpOutput {
    rejected_as(ctx, caller, input, expect, expect)
}

fn rejected_as(
    ctx: &mut TestCtx,
    caller: Address,
    input: &[u8],
    expect: &str,
    label: &str,
) -> PerpOutput {
    let _ = take_logs(ctx);
    let before = world(ctx);
    let writes = ctx.perp_write_count();
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(out.reverted, "[{label}] expected the revert `{expect}`, but the call SUCCEEDED");
    assert_eq!(revert_reason(&out), expect, "[{label}]");
    assert_eq!(ctx.perp_write_count(), writes, "[{label}] the rejected call wrote perp state");
    assert!(take_logs(ctx).is_empty(), "[{label}] the rejected call logged");
    assert!(world(ctx) == before, "[{label}] the rejected call changed stored state");
    out
}

/// A call refused at the gas gate: `Err(OutOfGas)` and nothing written.
fn out_of_gas(ctx: &mut TestCtx, caller: Address, input: &[u8], gas_limit: u64, label: &str) {
    let _ = take_logs(ctx);
    let before = world(ctx);
    let writes = ctx.perp_write_count();
    let err = run_perp_dex_call(input, gas_limit, caller, U256::ZERO, false, ctx).unwrap_err();
    assert!(matches!(err, PerpError::OutOfGas), "[{label}] {err:?}");
    assert_eq!(ctx.perp_write_count(), writes, "[{label}] an OutOfGas call wrote perp state");
    assert!(take_logs(ctx).is_empty(), "[{label}] an OutOfGas call logged");
    assert!(world(ctx) == before, "[{label}] an OutOfGas call changed stored state");
}

// ── Calldata builders ────────────────────────────────────────────────────────────────────

fn as_sub(sub: Address, inner: Vec<u8>) -> Vec<u8> {
    asSubAccountCall {
        subAccount: sub,
        data: inner.into(),
    }
    .abi_encode()
}

fn create_input(name: &str) -> Vec<u8> {
    createSubAccountCall {
        name: name.to_string(),
    }
    .abi_encode()
}

fn create(ctx: &mut TestCtx, master: Address, name: &str) -> Address {
    let (out, _) = ok(ctx, master, &create_input(name));
    createSubAccountCall::abi_decode_returns(&out.bytes).unwrap()
}

fn xfer(to: Address, from_wallet: u8, to_wallet: u8, amount: u64) -> Vec<u8> {
    internalTransferCall {
        to,
        fromWallet: from_wallet,
        toWallet: to_wallet,
        amount,
    }
    .abi_encode()
}

/// A GTC limit order.
fn order(side: u8, price: u64, qty: u64) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode()
}

fn cancel(id: [u8; 32]) -> Vec<u8> {
    cancelOrderCall {
        orderId: id.into(),
        marketId: MARKET_ID,
    }
    .abi_encode()
}

fn order_id_of(out: &PerpOutput) -> [u8; 32] {
    placeOrderCall::abi_decode_returns(&out.bytes).unwrap().0
}

fn set_fees(
    ctx: &mut TestCtx,
    user: Address,
    maker: i64,
    taker: u64,
) -> (PerpOutput, Vec<primitives::Log>) {
    ok(
        ctx,
        ADMIN,
        &setUserFeeRatesCall {
            user,
            makerFeePpm: maker,
            takerFeePpm: taker,
        }
        .abi_encode(),
    )
}

fn register_key(sk: &SigningKey, key_id: u8) -> Vec<u8> {
    registerApiKeyCall {
        keyId: key_id,
        pubkey: sk.verifying_key().to_bytes().into(),
        expiry: 0,
    }
    .abi_encode()
}

/// A reduce-only STOP_MARKET sell (the only kind of conditional this build admits).
fn stop_sell(trigger: u64, qty: u64) -> Vec<u8> {
    placeAlgoOrderCall {
        algoType: 0,
        marketId: MARKET_ID,
        side: SELL,
        orderType: AlgoOrderType::StopMarket as u8,
        triggerPrice: trigger,
        price: 0,
        quantity: qty,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags: 1,
    }
    .abi_encode()
}

/// `placeOrderSigned` for `account`: a GTC buy of 1 at `price`, signed by `sk` in slot
/// `key_id`. The message is rebuilt here from the ABI doc, independently of the engine.
fn signed_buy(
    ctx: &mut TestCtx,
    sk: &SigningKey,
    account: Address,
    price: u64,
    key_id: u8,
) -> (Vec<u8>, [u8; 64]) {
    let mut msg = Vec::new();
    msg.extend_from_slice(b"perpdex_v1_order");
    msg.extend_from_slice(&ctx.chain_id().to_be_bytes());
    msg.extend_from_slice(account.as_slice());
    msg.extend_from_slice(&MARKET_ID.to_be_bytes());
    msg.push(BUY);
    msg.extend_from_slice(&price.to_be_bytes());
    msg.extend_from_slice(&1u64.to_be_bytes());
    msg.extend_from_slice(&[0, 0]); // orderType, tif
    msg.extend_from_slice(&[0u8; 16]); // clientOrderId
    msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
    msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
    msg.extend_from_slice(&[key_id, 0]); // keyId, flags
    assert_eq!(msg.len(), 105);
    let sig = sk.sign(&msg).to_bytes();
    let input = placeOrderSignedCall {
        account,
        marketId: MARKET_ID,
        side: BUY,
        price,
        quantity: 1,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: key_id,
        flags: 0,
        signature: sig.to_vec().into(),
    }
    .abi_encode();
    (input, sig)
}

// ── State helpers ────────────────────────────────────────────────────────────────────────

/// Plants `spot` / `perp` on `user`, keeping every other field (a sub keeps its `master`).
fn seed(ctx: &mut TestCtx, user: Address, spot: u64, perp: i64) {
    let mut a = storage::load_account(ctx, user).unwrap();
    a.usdc_balance = U256::from(spot).into();
    a.perp_wallet_balance = perp;
    storage::save_account(ctx, user, a, AccountUpdateReason::Adjustment).unwrap();
}

fn seed_position(
    ctx: &mut TestCtx,
    user: Address,
    amount: i64,
    v_quote: i64,
    margin: i64,
    leverage: u64,
) {
    storage::save_position(
        ctx,
        user,
        MARKET_ID,
        &PerpPosition {
            amount,
            v_quote_balance: v_quote,
            margin,
            leverage,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

fn spot(ctx: &mut TestCtx, user: Address) -> u64 {
    let s: U256 = storage::load_account(ctx, user).unwrap().usdc_balance.into();
    s.to::<u64>()
}

fn perp(ctx: &mut TestCtx, user: Address) -> i64 {
    storage::load_account(ctx, user).unwrap().perp_wallet_balance
}

fn nonce(ctx: &mut TestCtx, user: Address) -> u64 {
    storage::load_user_nonce(ctx, user).unwrap()
}

fn fees(ctx: &mut TestCtx, user: Address) -> (i64, u64) {
    let r = storage::load_user_fee_rates(ctx, user).unwrap();
    (r.maker_fee_ppm, r.taker_fee_ppm)
}

/// Spec §5 conservation: `Σ usdc_balance + Σ perp_wallet_balance` over `users`.
fn money(ctx: &mut TestCtx, users: &[Address]) -> i128 {
    users
        .iter()
        .map(|&u| spot(ctx, u) as i128 + perp(ctx, u) as i128)
        .sum()
}

fn get_subs(ctx: &mut TestCtx, master: Address) -> Vec<Address> {
    let out = view(ctx, &getSubAccountsCall { master }.abi_encode());
    assert_eq!(out.gas_used, 5_000);
    getSubAccountsCall::abi_decode_returns(&out.bytes).unwrap()
}

fn get_master(ctx: &mut TestCtx, account: Address) -> Address {
    let out = view(ctx, &getMasterCall { account }.abi_encode());
    assert_eq!(out.gas_used, 5_000);
    getMasterCall::abi_decode_returns(&out.bytes).unwrap()
}

/// `getAccount(user).availableBalance` through the shell — the derived `wallet − Σ ooIM`.
fn available(ctx: &mut TestCtx, user: Address) -> i64 {
    let out = view(ctx, &getAccountCall { user }.abi_encode());
    getAccountCall::abi_decode_returns(&out.bytes)
        .unwrap()
        .availableBalance
}

/// `(user, reason, usdcBalance, totalWalletBalance, totalCrossWalletBalance)` per row.
fn balance_rows(logs: &[primitives::Log]) -> Vec<(Address, u8, U256, i64, i64)> {
    decode_events::<AccountBalanceChanged>(logs)
        .into_iter()
        .map(|e| {
            (
                e.user,
                e.reason,
                e.usdcBalance,
                e.totalWalletBalance,
                e.totalCrossWalletBalance,
            )
        })
        .collect()
}

/// Batch status records `tag(1) ‖ orderId(32) ‖ reason(1)`.
fn batch_records(blob: &[u8]) -> Vec<(u8, [u8; 32], u8)> {
    assert_eq!(blob.len() % 34, 0);
    blob.chunks(34)
        .map(|r| (r[0], r[1..33].try_into().unwrap(), r[33]))
        .collect()
}

// ═════════════════════════════════════════════════════════════════════════════════════════
// §1 Identity
// ═════════════════════════════════════════════════════════════════════════════════════════

/// §1 / §9 "derivation: deterministic, distinct per (master, index), matches a hand-computed
/// vector". The formula is restated from the spec with no engine helper, then both it and the
/// engine's `derive_sub_account` are held to the ethers-computed literals.
#[test]
fn derivation_matches_the_spec_formula_and_independent_vectors() {
    for (master, index, expected) in [
        (MA, 0u8, SA0),
        (MA, 1, SA1),
        (MA, 2, SA2),
        (MA, 9, SA9),
        (MA, 10, SA10),
        (MB, 0, SB0),
    ] {
        let mut preimage = Vec::new();
        preimage.extend_from_slice(b"perpdex_subaccount_v1");
        preimage.extend_from_slice(master.as_slice());
        preimage.push(index);
        assert_eq!(preimage.len(), 21 + 20 + 1);
        assert_eq!(Address::from_slice(&keccak256(&preimage)[12..]), expected);
        assert_eq!(derive_sub_account(master, index), expected, "{master} #{index}");
        assert_eq!(derive_sub_account(master, index), expected, "deterministic");
    }
    let mut all = BTreeSet::from([MA, MB, X]);
    for m in [MA, MB, X] {
        for i in 0..MAX_SUB_ACCOUNTS {
            assert!(all.insert(derive_sub_account(m, i)), "collision at {m} #{i}");
        }
    }
    assert_eq!(all.len(), 3 + 3 * 10, "distinct per (master, index), and never a master");
    assert_eq!(MAX_SUB_ACCOUNTS, 10, "decision 2");
}

// ═════════════════════════════════════════════════════════════════════════════════════════
// §2 createSubAccount
// ═════════════════════════════════════════════════════════════════════════════════════════

/// §2 / §6 / §9 "create: sequential indices, 11th rejected, fee copy, event" + the two views.
#[test]
fn create_assigns_sequential_derived_subs_copies_fees_and_stops_at_ten() {
    let mut ctx = ctx();
    set_fees(&mut ctx, MA, 200, 500);
    for i in 0..MAX_SUB_ACCOUNTS {
        let name = format!("desk-{i}");
        let (out, logs) = ok(&mut ctx, MA, &create_input(&name));
        assert_eq!(out.gas_used, 50_000);
        let sub = createSubAccountCall::abi_decode_returns(&out.bytes).unwrap();
        assert_eq!(out.bytes.len(), 32, "returns exactly one ABI address word");
        match i {
            0 => assert_eq!(sub, SA0),
            1 => assert_eq!(sub, SA1),
            2 => assert_eq!(sub, SA2),
            9 => assert_eq!(sub, SA9),
            _ => assert_eq!(sub, derive_sub_account(MA, i)),
        }
        // Exactly ONE log: no balance moved, so no AccountBalanceChanged.
        assert_eq!(logs.len(), 1, "#{i}");
        let ev = decode_events::<SubAccountCreated>(&logs);
        assert_eq!(
            (ev[0].master, ev[0].subAccount, ev[0].index, ev[0].name.as_str()),
            (MA, sub, i, name.as_str())
        );
        // The sub is a fresh account whose only content is its master and the copied rates.
        assert_eq!(
            storage::load_account(&mut ctx, sub).unwrap(),
            UserAccount {
                master: Some(MA),
                maker_fee_ppm: 200,
                taker_fee_ppm: 500,
                ..UserAccount::default()
            },
            "#{i}"
        );
        assert_eq!(storage::load_account(&mut ctx, MA).unwrap().sub_count, i + 1);
    }
    assert_eq!(
        storage::load_account(&mut ctx, MA).unwrap(),
        UserAccount {
            maker_fee_ppm: 200,
            taker_fee_ppm: 500,
            sub_count: 10,
            ..UserAccount::default()
        },
        "the master's blob gained exactly the count"
    );

    // §6 views
    let subs = get_subs(&mut ctx, MA);
    assert_eq!(subs.len(), 10);
    assert_eq!(&subs[..3], &[SA0, SA1, SA2]);
    assert_eq!(subs[9], SA9);
    assert_eq!(subs, (0..10).map(|i| derive_sub_account(MA, i)).collect::<Vec<_>>());
    assert_eq!(get_subs(&mut ctx, SA0), Vec::<Address>::new(), "a sub has no subs");
    assert_eq!(get_subs(&mut ctx, MB), Vec::<Address>::new());
    for s in subs {
        assert_eq!(get_master(&mut ctx, s), MA);
    }
    assert_eq!(get_master(&mut ctx, MA), Address::ZERO, "a master has no master");
    assert_eq!(get_master(&mut ctx, X), Address::ZERO);
    assert_eq!(get_master(&mut ctx, SA10), Address::ZERO);

    // The 11th: refused, and index 10 stays untouched.
    rejected(&mut ctx, MA, &create_input("desk-10"), E_CREATE_CAP);
    assert_eq!(storage::load_account(&mut ctx, SA10).unwrap(), UserAccount::default());
    assert_eq!(storage::load_account(&mut ctx, MA).unwrap().sub_count, 10);

    // Another master is unaffected by A's cap.
    assert_eq!(create(&mut ctx, MB, "b"), SB0);
}

/// §1 "a sub cannot create subs; an address that is a sub can never become a master" + the
/// name rule the implementation chose (1..=32 BYTES, not chars).
#[test]
fn a_sub_cannot_create_and_the_name_is_one_to_thirty_two_bytes() {
    let mut ctx = ctx();
    assert_eq!(create(&mut ctx, MA, "trading"), SA0);

    // Directly as the sub (it has no key — the rule must hold regardless) …
    rejected(&mut ctx, SA0, &create_input("nested"), E_CREATE_BY_SUB);
    // … and through the wrapper the selector never reaches the handler.
    rejected(&mut ctx, MA, &as_sub(SA0, create_input("nested")), E_DENIED);
    assert_eq!(get_subs(&mut ctx, SA0), Vec::<Address>::new());
    assert_eq!(get_master(&mut ctx, SA0), MA);
    assert_eq!(
        storage::load_account(&mut ctx, derive_sub_account(SA0, 0)).unwrap(),
        UserAccount::default()
    );

    rejected(&mut ctx, MA, &create_input(""), E_NAME_EMPTY);
    rejected(&mut ctx, MA, &create_input(&"x".repeat(33)), E_NAME_LONG);
    // BYTES, not chars: 11 × the 3-byte "账" is 33 bytes.
    rejected(&mut ctx, MA, &create_input(&"账".repeat(11)), E_NAME_LONG);
    assert_eq!(storage::load_account(&mut ctx, MA).unwrap().sub_count, 1, "no index burned");

    let cjk_32 = format!("{}ab", "账".repeat(10));
    assert_eq!(cjk_32.len(), 32);
    let (out, logs) = ok(&mut ctx, MA, &create_input(&cjk_32));
    assert_eq!(createSubAccountCall::abi_decode_returns(&out.bytes).unwrap(), SA1);
    assert_eq!(decode_events::<SubAccountCreated>(&logs)[0].name, cjk_32);
    let (_, logs) = ok(&mut ctx, MA, &create_input("y"));
    let ev = decode_events::<SubAccountCreated>(&logs);
    assert_eq!((ev[0].subAccount, ev[0].index, ev[0].name.as_str()), (SA2, 2, "y"));
    assert_eq!(get_subs(&mut ctx, MA), vec![SA0, SA1, SA2]);
}

/// §2 guard — every kind of state that only the derived address's own KEY (or an earlier
/// family link) could have produced refuses the create, write-free.
#[test]
fn create_refuses_a_derived_address_that_already_has_keyed_engine_state() {
    let cases: [(&str, fn(&mut TestCtx)); 6] = [
        ("perp wallet balance", |c| seed(c, SA0, 0, 1)),
        ("nonce", |c| storage::save_user_nonce(c, SA0, 1).unwrap()),
        ("api key", |c| {
            storage::save_api_key(c, SA0, 0, ApiKey { pubkey: [7u8; 32], expiry: 0 }).unwrap()
        }),
        ("open position (market index entry)", |c| {
            seed_position(c, SA0, 1, -10_000_000, 10_000_000, 1)
        }),
        ("already a sub of another family", |c| {
            let mut a = storage::load_account(c, SA0).unwrap();
            a.master = Some(MB);
            storage::save_account(c, SA0, a, AccountUpdateReason::Adjustment).unwrap();
        }),
        ("already a master", |c| {
            let mut a = storage::load_account(c, SA0).unwrap();
            a.sub_count = 1;
            storage::save_account(c, SA0, a, AccountUpdateReason::Adjustment).unwrap();
        }),
    ];
    for (what, plant) in cases {
        let mut ctx = ctx();
        plant(&mut ctx);
        rejected_as(&mut ctx, MA, &create_input("desk"), E_CREATE_CLAIMED, what);
        assert_eq!(storage::load_account(&mut ctx, MA).unwrap().sub_count, 0, "{what}");
        assert_eq!(get_subs(&mut ctx, MA), Vec::<Address>::new(), "{what}");
    }
}

/// §2 guard, AMENDED: a gifted SPOT balance on the next derived address does NOT refuse the
/// create — the new sub adopts it as its own money.
///
/// Before it is created, `derive(MA, sub_count)` is an ordinary address, so it is a LEGAL external
/// SPOT→SPOT recipient (≥ 1 USDC), and it is public (`derive(MA, getSubAccounts(MA).length)`). The
/// spec's original literal rule ("refuse ANY non-default `UserAccount`") therefore let anyone block
/// MA's next — and, indices being sequential, every later — sub-account for 1 USDC, so §2 now
/// refuses only state that needs the address's own key (see
/// `create_refuses_a_derived_address_that_already_has_keyed_engine_state`). The flip side, pinned
/// here too: a sub can be BORN holding third-party money; "no third party → sub" binds only once
/// the sub exists.
#[test]
fn create_adopts_a_gifted_spot_balance_as_the_new_subs_money() {
    let mut ctx = ctx();
    set_fees(&mut ctx, MA, 200, 500);
    seed(&mut ctx, X, 5 * ONE_USDC, 0);
    ok(&mut ctx, X, &xfer(SA0, SPOT, SPOT, ONE_USDC));
    assert_eq!(spot(&mut ctx, SA0), ONE_USDC);
    assert_ne!(storage::load_account(&mut ctx, SA0).unwrap(), UserAccount::default());

    let (out, logs) = ok(&mut ctx, MA, &create_input("desk"));
    assert_eq!(createSubAccountCall::abi_decode_returns(&out.bytes).unwrap(), SA0);
    assert_eq!(logs.len(), 1, "only SubAccountCreated: adopting the gift moves no balance");
    assert_eq!(
        storage::load_account(&mut ctx, SA0).unwrap(),
        UserAccount {
            usdc_balance: U256::from(ONE_USDC).into(),
            master: Some(MA),
            maker_fee_ppm: 200,
            taker_fee_ppm: 500,
            ..UserAccount::default()
        },
        "the gift is the new sub's SPOT balance; everything else is a fresh sub"
    );
    assert_eq!(get_master(&mut ctx, SA0), MA);
    assert_eq!(get_subs(&mut ctx, MA), vec![SA0]);
    assert_eq!(spot(&mut ctx, X), 4 * ONE_USDC);

    // Now that SA0 IS a sub, the same gift is refused...
    rejected(&mut ctx, X, &xfer(SA0, SPOT, SPOT, ONE_USDC), E_XFER_TO_FOREIGN_SUB);
    // ...and the adopted money is the family's: MA can pull it home.
    ok(&mut ctx, MA, &as_sub(SA0, xfer(MA, SPOT, SPOT, ONE_USDC)));
    assert_eq!((spot(&mut ctx, SA0), spot(&mut ctx, MA)), (0, ONE_USDC));
}

/// §2 guard, AMENDED: admin-set fee rates already on the derived address do not refuse the create
/// either — they are OVERWRITTEN by the master's (§4: a sub's rates are its master's). Only the
/// admin can put them there (`setUserFeeRates` accepts any non-sub address), so this is no griefing
/// vector; refusing would only let an admin slip block a master's sub-accounts.
#[test]
fn create_overwrites_admin_fee_rates_preset_on_the_derived_address() {
    let mut ctx = ctx();
    set_fees(&mut ctx, MA, 200, 500);
    set_fees(&mut ctx, SA0, 100, 100);
    assert_ne!(storage::load_account(&mut ctx, SA0).unwrap(), UserAccount::default());

    assert_eq!(create(&mut ctx, MA, "desk"), SA0);
    assert_eq!(fees(&mut ctx, SA0), (200, 500), "the master's rates, not the preset");
    assert_eq!(
        storage::load_account(&mut ctx, SA0).unwrap(),
        UserAccount {
            master: Some(MA),
            maker_fee_ppm: 200,
            taker_fee_ppm: 500,
            ..UserAccount::default()
        }
    );
    // From now on the admin can no longer address the sub's rates directly.
    rejected(
        &mut ctx,
        ADMIN,
        &setUserFeeRatesCall {
            user: SA0,
            makerFeePpm: 100,
            takerFeePpm: 100,
        }
        .abi_encode(),
        E_FEE_SUB,
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════
// §4 Fee rates
// ═════════════════════════════════════════════════════════════════════════════════════════

/// §4 / §9 "fee push-down: admin set on master updates all subs; set on a sub rejected".
#[test]
fn fee_rates_are_copied_at_create_and_pushed_down_by_the_admin() {
    let mut ctx = ctx();
    set_fees(&mut ctx, MA, 200, 500);
    set_fees(&mut ctx, MB, 900, 900);
    for want in [SA0, SA1, SA2] {
        assert_eq!(create(&mut ctx, MA, "d"), want);
        assert_eq!(fees(&mut ctx, want), (200, 500), "copied at create");
    }
    assert_eq!(create(&mut ctx, MB, "d"), SB0);
    assert_eq!(fees(&mut ctx, SB0), (900, 900));

    // The push-down: one write + one event per family member, master first, subs in order.
    let (out, logs) = set_fees(&mut ctx, MA, 300, 700);
    assert_eq!(out.gas_used, 80_000);
    assert_eq!(logs.len(), 4, "fee rates are not a balance: no AccountBalanceChanged");
    let rows: Vec<_> = decode_events::<UserFeeRatesUpdated>(&logs)
        .into_iter()
        .map(|e| (e.user, e.makerFeePpm, e.takerFeePpm))
        .collect();
    assert_eq!(rows, vec![(MA, 300, 700), (SA0, 300, 700), (SA1, 300, 700), (SA2, 300, 700)]);
    for u in [MA, SA0, SA1, SA2] {
        assert_eq!(fees(&mut ctx, u), (300, 700));
        let v = view(&mut ctx, &getUserFeeRatesCall { user: u }.abi_encode());
        let r = getUserFeeRatesCall::abi_decode_returns(&v.bytes).unwrap();
        assert_eq!((r.makerFeePpm, r.takerFeePpm), (300, 700));
    }
    // The push-down touched only the fee fields: a sub's blob is still {master, rates}.
    assert_eq!(
        storage::load_account(&mut ctx, SA1).unwrap(),
        UserAccount {
            master: Some(MA),
            maker_fee_ppm: 300,
            taker_fee_ppm: 700,
            ..UserAccount::default()
        }
    );
    // The other family is not a member.
    assert_eq!(fees(&mut ctx, MB), (900, 900));
    assert_eq!(fees(&mut ctx, SB0), (900, 900));

    // Setting a SUB directly is refused, write-free.
    for sub in [SA0, SA2, SB0] {
        rejected(
            &mut ctx,
            ADMIN,
            &setUserFeeRatesCall {
                user: sub,
                makerFeePpm: 0,
                takerFeePpm: 0,
            }
            .abi_encode(),
            E_FEE_SUB,
        );
    }
    // A sub created AFTER the push-down inherits the CURRENT rates.
    let sa3 = create(&mut ctx, MA, "late");
    assert_eq!(sa3, derive_sub_account(MA, 3));
    assert_eq!(fees(&mut ctx, sa3), (300, 700));
    // An ordinary user: one write, one event — the pre-P1 behaviour.
    let (_, logs) = set_fees(&mut ctx, X, 100, 100);
    let rows: Vec<_> = decode_events::<UserFeeRatesUpdated>(&logs)
        .into_iter()
        .map(|e| (e.user, e.makerFeePpm, e.takerFeePpm))
        .collect();
    assert_eq!(rows, vec![(X, 100, 100)]);
}

// ═════════════════════════════════════════════════════════════════════════════════════════
// §3 asSubAccount
// ═════════════════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug)]
enum Wrap {
    /// Whitelisted (spec §3 ALLOW). Fed a bare selector it reaches the INNER handler, which
    /// rejects with its own calldata error; billed `inner_gas + 5_000`.
    Allow {
        inner_reject: &'static str,
        inner_gas: u64,
    },
    /// `deposit` / `withdraw` — refused with their own message (decision 4).
    DenyMoneyInOut,
    /// Every other selector (spec §3 DENY).
    Deny,
}

/// **The spec's classification of EVERY selector in the shell's table.** The test below
/// asserts this table and `selectors_map()` have the SAME key set, so adding a selector to the
/// table without classifying it here fails — a new selector can never become reachable through
/// the wrapper silently.
fn spec_wrap_table() -> Vec<(&'static str, [u8; 4], Wrap)> {
    use Wrap::*;
    let allow = |inner_reject, inner_gas| Allow {
        inner_reject,
        inner_gas,
    };
    vec![
        // ── ALLOW — spec §3, verbatim ──
        ("placeOrder", placeOrderCall::SELECTOR, allow("placeOrder: invalid calldata", 200_000)),
        ("cancelOrder", cancelOrderCall::SELECTOR, allow("cancelOrder: invalid calldata", 80_000)),
        // A bare selector has no length word, so the batch bills only its 20_000 floor.
        ("batchPlaceOrders", batchPlaceOrdersCall::SELECTOR, allow("batch: truncated calldata (array head)", 20_000)),
        ("batchCancelOrders", batchCancelOrdersCall::SELECTOR, allow("batch: truncated calldata (array head)", 20_000)),
        ("placeAlgoOrder", placeAlgoOrderCall::SELECTOR, allow("placeAlgoOrder: invalid calldata", 200_000)),
        ("cancelAlgoOrder", cancelAlgoOrderCall::SELECTOR, allow("cancelAlgoOrder: invalid calldata", 80_000)),
        ("setLeverage", setLeverageCall::SELECTOR, allow("setLeverage: invalid calldata", 30_000)),
        // Cross P1 — added to the whitelist with the selector.
        ("setMarginType", setMarginTypeCall::SELECTOR, allow("setMarginType: invalid calldata", 30_000)),
        ("addPositionMargin", addPositionMarginCall::SELECTOR, allow("addPositionMargin: invalid calldata", 40_000)),
        ("removePositionMargin", removePositionMarginCall::SELECTOR, allow("removePositionMargin: invalid calldata", 40_000)),
        ("transferToPerp", transferToPerpCall::SELECTOR, allow("transferToPerp: invalid calldata", 30_000)),
        ("transferFromPerp", transferFromPerpCall::SELECTOR, allow("transferFromPerp: invalid calldata", 30_000)),
        ("registerApiKey", registerApiKeyCall::SELECTOR, allow("registerApiKey: invalid calldata", 30_000)),
        ("revokeApiKey", revokeApiKeyCall::SELECTOR, allow("revokeApiKey: invalid calldata", 20_000)),
        ("internalTransfer", internalTransferCall::SELECTOR, allow("internalTransfer: invalid calldata", 50_000)),
        // ── DENY: ERC-20 legs keyed to the caller (decision 4) ──
        ("deposit", depositCall::SELECTOR, DenyMoneyInOut),
        ("withdraw", withdrawCall::SELECTOR, DenyMoneyInOut),
        // ── DENY: no nesting / no recursion ──
        ("createSubAccount", createSubAccountCall::SELECTOR, Deny),
        ("asSubAccount", asSubAccountCall::SELECTOR, Deny),
        // ── DENY: every *Signed selector (they authenticate `account`, not the caller) ──
        ("placeOrderSigned", placeOrderSignedCall::SELECTOR, Deny),
        ("cancelOrderSigned", cancelOrderSignedCall::SELECTOR, Deny),
        ("batchPlaceOrdersSigned", batchPlaceOrdersSignedCall::SELECTOR, Deny),
        ("batchCancelOrdersSigned", batchCancelOrdersSignedCall::SELECTOR, Deny),
        ("placeAlgoOrderSigned", placeAlgoOrderSignedCall::SELECTOR, Deny),
        ("cancelAlgoOrderSigned", cancelAlgoOrderSignedCall::SELECTOR, Deny),
        ("setLeverageSigned", setLeverageSignedCall::SELECTOR, Deny),
        ("setMarginTypeSigned", setMarginTypeSignedCall::SELECTOR, Deny),
        ("transferToPerpSigned", transferToPerpSignedCall::SELECTOR, Deny),
        ("transferFromPerpSigned", transferFromPerpSignedCall::SELECTOR, Deny),
        // ── DENY: admin / role / oracle / market-manager / market-config / insurance fund ──
        ("initAdmin", initAdminCall::SELECTOR, Deny),
        ("transferAdmin", transferAdminCall::SELECTOR, Deny),
        ("setUserFeeRates", setUserFeeRatesCall::SELECTOR, Deny),
        ("setMaxMakerRebate", setMaxMakerRebateCall::SELECTOR, Deny),
        ("setMarketManagerAddress", setMarketManagerAddressCall::SELECTOR, Deny),
        ("setOracleAddress", setOracleAddressCall::SELECTOR, Deny),
        ("addMarket", addMarketCall::SELECTOR, Deny),
        ("updateMarket", updateMarketCall::SELECTOR, Deny),
        ("setMarginTiers", setMarginTiersCall::SELECTOR, Deny),
        ("setBasisMode", setBasisModeCall::SELECTOR, Deny),
        ("updateIndexPrice", updateIndexPriceCall::SELECTOR, Deny),
        ("updateIndexPriceFromBook", updateIndexPriceFromBookCall::SELECTOR, Deny),
        ("depositInsuranceFund", depositInsuranceFundCall::SELECTOR, Deny),
        ("withdrawInsuranceFund", withdrawInsuranceFundCall::SELECTOR, Deny),
        // ── DENY: permissionless protocol work, not per-caller ──
        ("liquidate", liquidateCall::SELECTOR, Deny),
        ("drainDeferredWork", drainDeferredWorkCall::SELECTOR, Deny),
        // ── DENY: every view ──
        ("getAdmin", getAdminCall::SELECTOR, Deny),
        ("getAccount", getAccountCall::SELECTOR, Deny),
        ("getUserFeeRates", getUserFeeRatesCall::SELECTOR, Deny),
        ("getMaxMakerRebate", getMaxMakerRebateCall::SELECTOR, Deny),
        ("getMarketFeeTotal", getMarketFeeTotalCall::SELECTOR, Deny),
        ("getSubAccounts", getSubAccountsCall::SELECTOR, Deny),
        ("getMaster", getMasterCall::SELECTOR, Deny),
        ("getMarketManagerAddress", getMarketManagerAddressCall::SELECTOR, Deny),
        ("getOracleAddress", getOracleAddressCall::SELECTOR, Deny),
        ("getMarkPrice", getMarkPriceCall::SELECTOR, Deny),
        ("getMarket", getMarketCall::SELECTOR, Deny),
        ("getMarginTiers", getMarginTiersCall::SELECTOR, Deny),
        ("getSymbolConfig", getSymbolConfigCall::SELECTOR, Deny),
        ("getOrder", getOrderCall::SELECTOR, Deny),
        ("getOpenOrders", getOpenOrdersCall::SELECTOR, Deny),
        ("getBookPrices", getBookPricesCall::SELECTOR, Deny),
        ("getBookLevel", getBookLevelCall::SELECTOR, Deny),
        ("getPosition", getPositionCall::SELECTOR, Deny),
        ("getPositionRisk", getPositionRiskCall::SELECTOR, Deny),
        ("getAccountMargin", getAccountMarginCall::SELECTOR, Deny),
        ("getApiKey", getApiKeyCall::SELECTOR, Deny),
        ("getApiKeys", getApiKeysCall::SELECTOR, Deny),
        ("getInsuranceFund", getInsuranceFundCall::SELECTOR, Deny),
        ("getAlgoOrder", getAlgoOrderCall::SELECTOR, Deny),
        ("getOpenAlgoOrders", getOpenAlgoOrdersCall::SELECTOR, Deny),
        ("getIndexPrice", getIndexPriceCall::SELECTOR, Deny),
        ("getIndexMode", getIndexModeCall::SELECTOR, Deny),
        ("getFundingState", getFundingStateCall::SELECTOR, Deny),
        ("getAveragePremiumIndex", getAveragePremiumIndexCall::SELECTOR, Deny),
    ]
}

/// §3 / §9 "every DENY-listed selector rejected", table-driven over the shell's ENTIRE selector
/// table: allow/deny must match the spec row for row, a denied row is billed only the 5_000
/// floor and writes nothing, an allowed row demonstrably reaches its own handler.
#[test]
fn the_as_sub_account_whitelist_matches_the_spec_for_every_selector_in_the_table() {
    let table = spec_wrap_table();
    let names: BTreeSet<&str> = table.iter().map(|r| r.0).collect();
    assert_eq!(names.len(), table.len(), "duplicate name in the spec table");
    let ours: BTreeSet<[u8; 4]> = table.iter().map(|r| r.1).collect();
    assert_eq!(ours.len(), table.len(), "duplicate selector in the spec table");
    let shell: BTreeSet<[u8; 4]> = selectors_map().keys().copied().collect();
    assert_eq!(
        ours, shell,
        "the shell's selector table and the spec classification differ — classify the new \
         selector here (default DENY) before it ships"
    );
    assert_eq!(table.len(), 74);
    assert_eq!(table.iter().filter(|r| matches!(r.2, Wrap::Allow { .. })).count(), 15);
    // `internalTransfer` is not key-signable (spec §5): no signed twin exists.
    assert!(!names.iter().any(|n| n.starts_with("internalTransfer") && n.ends_with("Signed")));

    let mut ctx = ctx();
    assert_eq!(create(&mut ctx, MA, "desk"), SA0);
    // Money on the sub, so a misrouted money selector would have something to move.
    seed(&mut ctx, SA0, 1_000 * ONE_USDC, 1_000 * ONE_USDC as i64);
    for (name, selector, class) in table {
        let input = as_sub(SA0, selector.to_vec());
        let out = match class {
            Wrap::Deny => rejected_as(&mut ctx, MA, &input, E_DENIED, name),
            Wrap::DenyMoneyInOut => rejected_as(&mut ctx, MA, &input, E_DENIED_DW, name),
            Wrap::Allow { inner_reject, .. } => {
                rejected_as(&mut ctx, MA, &input, inner_reject, name)
            }
        };
        let want_gas = match class {
            Wrap::Allow { inner_gas, .. } => inner_gas + OVERHEAD,
            _ => OVERHEAD,
        };
        assert_eq!(out.gas_used, want_gas, "[{name}] gas");
    }

    // Not in the table at all: default-deny, floor gas.
    let out = rejected(&mut ctx, MA, &as_sub(SA0, vec![0xde, 0xad, 0xbe, 0xef]), E_DENIED);
    assert_eq!(out.gas_used, OVERHEAD);
    // No selector at all.
    rejected(&mut ctx, MA, &as_sub(SA0, vec![]), E_NO_SELECTOR);
    rejected(&mut ctx, MA, &as_sub(SA0, vec![0x01, 0x02, 0x03]), E_NO_SELECTOR);
}

/// §3 / §9 "non-master rejected; master of a DIFFERENT family rejected; no recursion" + a sub
/// calling the wrapper, the EOA guard on the OUTER call, the static gate, and the inner
/// result / revert passing through unchanged.
#[test]
fn only_the_master_may_act_for_its_sub_and_the_wrapper_cannot_nest() {
    let mut ctx = ctx();
    assert_eq!(create(&mut ctx, MA, "a"), SA0);
    assert_eq!(create(&mut ctx, MA, "b"), SA1);
    assert_eq!(create(&mut ctx, MB, "c"), SB0);
    for u in [SA0, SA1, SB0] {
        seed(&mut ctx, u, 100 * ONE_USDC, 0);
    }
    let inner = || transferToPerpCall { amount: 10 }.abi_encode();
    for (caller, target, who) in [
        (X, SA0, "a stranger"),
        (MB, SA0, "the master of ANOTHER family"),
        (SB0, SA0, "another family's sub"),
        (SA1, SA0, "a sibling sub"),
        (SA0, SA0, "the sub itself"),
        (SA0, SA1, "a sub, for its sibling"),
        (MA, SB0, "a master, for another family's sub"),
        (MA, MA, "a master, on itself"),
        (MA, X, "a master, on an ordinary account"),
        (MA, SA2, "a master, on its NEXT (uncreated) derived address"),
        (MA, Address::ZERO, "a master, on the zero address"),
    ] {
        let out = rejected_as(&mut ctx, caller, &as_sub(target, inner()), E_NOT_MASTER, who);
        // Billed inner + overhead: the charge is decided pre-dispatch from the calldata.
        assert_eq!(out.gas_used, 30_000 + OVERHEAD, "[{who}]");
    }

    // No recursion — not even with a perfectly valid inner wrapper.
    let out = rejected(&mut ctx, MA, &as_sub(SA0, as_sub(SA0, inner())), E_DENIED);
    assert_eq!(out.gas_used, OVERHEAD);
    rejected(&mut ctx, MA, &as_sub(SA0, as_sub(SA1, inner())), E_DENIED);

    // The positive control: the master, acting as SA0.
    let ma_before = owned(&mut ctx, MA);
    let (out, logs) = ok(&mut ctx, MA, &as_sub(SA0, inner()));
    assert_eq!(out.gas_used, 35_000);
    assert!(out.bytes.is_empty(), "transferToPerp returns nothing, and neither does the wrap");
    assert_eq!((spot(&mut ctx, SA0), perp(&mut ctx, SA0)), (100 * ONE_USDC - 10, 10));
    assert!(owned(&mut ctx, MA) == ma_before, "nothing landed on the master");
    let t = decode_events::<TransferToPerp>(&logs);
    assert_eq!((t.len(), t[0].user, t[0].amount), (1, SA0, 10));
    assert_eq!(account_update_reasons(&logs), vec![(SA0, R::AssetTransfer)]);

    // An inner revert is passed through VERBATIM, write-free.
    rejected(&mut ctx, MA, &as_sub(SA0, transferFromPerpCall { amount: 11 }.abi_encode()), E_FROM_PERP);
    rejected(
        &mut ctx,
        MA,
        &as_sub(SA0, transferToPerpCall { amount: 0 }.abi_encode()),
        "transferToPerp: amount must be > 0",
    );

    // EOA-direct only: a contract-mediated OUTER call is refused. The guard returns the reject
    // straight out of the shell (the revm adapter turns it into the revert), before any write.
    ctx.journal_mut().inner.depth = 2;
    let _ = take_logs(&mut ctx);
    let before = world(&mut ctx);
    let writes = ctx.perp_write_count();
    let err = run_perp_dex_call(&as_sub(SA0, inner()), GAS, MA, U256::ZERO, false, &mut ctx)
        .unwrap_err();
    assert!(matches!(&err, PerpError::Reject(m) if m == E_EOA), "{err:?}");
    assert_eq!(ctx.perp_write_count(), writes);
    assert!(take_logs(&mut ctx).is_empty());
    assert!(world(&mut ctx) == before);
    ctx.journal_mut().inner.depth = 0;

    // Not static-callable.
    let writes = ctx.perp_write_count();
    let err = run_perp_dex_call(&as_sub(SA0, inner()), GAS, MA, U256::ZERO, true, &mut ctx)
        .unwrap_err();
    assert!(matches!(err, PerpError::StaticRestrictionViolation), "{err:?}");
    assert_eq!(ctx.perp_write_count(), writes);
}

/// §9 "whitelisted ones act AS the sub (positions/orders/keys land on the sub, not the
/// master)" — every allowed selector, with real arguments, in one lifecycle.
#[test]
fn every_whitelisted_action_lands_on_the_sub_and_never_on_the_master() {
    let mut ctx = ctx();
    seed(&mut ctx, MA, 10_000 * ONE_USDC, 0);
    assert_eq!(create(&mut ctx, MA, "desk"), SA0);
    ok(&mut ctx, MA, &xfer(SA0, SPOT, SPOT, 3_000 * ONE_USDC));
    let ma = owned(&mut ctx, MA);
    let wrap = |ctx: &mut TestCtx, inner: Vec<u8>| ok(ctx, MA, &as_sub(SA0, inner));

    // transferToPerp
    wrap(&mut ctx, transferToPerpCall { amount: 2_000 * ONE_USDC }.abi_encode());
    assert_eq!((spot(&mut ctx, SA0), perp(&mut ctx, SA0)), (1_000 * ONE_USDC, 2_000 * ONE_USDC as i64));

    // setLeverage
    wrap(&mut ctx, setLeverageCall { marketId: MARKET_ID, leverage: 2 }.abi_encode());
    assert_eq!(position(&mut ctx, SA0).leverage, 2);

    // placeOrder — a taker buy against MAKER's ask; the inner return data comes back verbatim.
    place_maker_order(&mut ctx, SELL, ENTRY_PRICE, 10);
    let (out, logs) = wrap(&mut ctx, order(BUY, ENTRY_PRICE, 10));
    assert_eq!(out.bytes.to_vec(), placeOrderCall::abi_encode_returns(&FixedBytes(OID_SA0_0)));
    let placed = decode_events::<OrderPlaced>(&logs);
    assert_eq!((placed[0].user, placed[0].orderId.0), (SA0, OID_SA0_0));
    let trade = decode_events::<Trade>(&logs);
    assert_eq!(
        (trade[0].taker, trade[0].maker, trade[0].price, trade[0].quantity),
        (SA0, MAKER, ENTRY_PRICE, 10)
    );
    let p = position(&mut ctx, SA0);
    assert_eq!((p.amount, p.v_quote_balance, p.margin, p.leverage), (10, -1_000_000_000, 500_000_000, 2));
    assert_eq!(perp(&mut ctx, SA0), 1_500_000_000);
    assert_eq!((nonce(&mut ctx, SA0), nonce(&mut ctx, MA)), (1, 0));

    // addPositionMargin / removePositionMargin
    wrap(&mut ctx, addPositionMarginCall { marketId: MARKET_ID, amount: 100_000_000 }.abi_encode());
    assert_eq!((position(&mut ctx, SA0).margin, perp(&mut ctx, SA0)), (600_000_000, 1_400_000_000));
    wrap(&mut ctx, removePositionMarginCall { marketId: MARKET_ID, amount: 50_000_000 }.abi_encode());
    assert_eq!((position(&mut ctx, SA0).margin, perp(&mut ctx, SA0)), (550_000_000, 1_450_000_000));

    // placeAlgoOrder / cancelAlgoOrder — the id comes from the SUB's nonce (now 1).
    let (out, _) = wrap(&mut ctx, stop_sell(8_000, 10));
    let algo_id: [u8; 32] = out.bytes[..32].try_into().unwrap();
    assert_eq!(algo_id, OID_SA0_1);
    assert_eq!(storage::load_user_algo_orders(&mut ctx, SA0, MARKET_ID).unwrap(), vec![OID_SA0_1]);
    wrap(&mut ctx, cancelAlgoOrderCall { algoId: algo_id.into(), marketId: MARKET_ID }.abi_encode());
    assert!(storage::load_user_algo_orders(&mut ctx, SA0, MARKET_ID).unwrap().is_empty());

    // placeOrder (resting) / cancelOrder
    let (out, _) = wrap(&mut ctx, order(BUY, 9_000, 1));
    assert_eq!(order_id_of(&out), OID_SA0_2);
    let o = storage::load_order(&mut ctx, &OID_SA0_2).unwrap().unwrap();
    assert_eq!(Address::from(o.owner), SA0);
    assert_eq!(storage::load_bid_count(&mut ctx, MARKET_ID, 9_000).unwrap(), 1);
    wrap(&mut ctx, cancel(OID_SA0_2));
    assert!(storage::load_order(&mut ctx, &OID_SA0_2).unwrap().is_none());
    assert_eq!(storage::load_bid_count(&mut ctx, MARKET_ID, 9_000).unwrap(), 0);

    // registerApiKey / revokeApiKey — into the SUB's slot (decision 3).
    let sk = SigningKey::from_bytes(&[0x41; 32]);
    let (_, logs) = wrap(&mut ctx, register_key(&sk, 3));
    let reg = decode_events::<ApiKeyRegistered>(&logs);
    assert_eq!((reg[0].user, reg[0].keyId, reg[0].pubkey.0), (SA0, 3, sk.verifying_key().to_bytes()));
    assert_eq!(
        storage::load_api_key(&mut ctx, SA0, 3).unwrap(),
        Some(ApiKey { pubkey: sk.verifying_key().to_bytes(), expiry: 0 })
    );
    assert_eq!(storage::load_api_key(&mut ctx, MA, 3).unwrap(), None);
    wrap(&mut ctx, revokeApiKeyCall { keyId: 3 }.abi_encode());
    assert_eq!(storage::load_api_key(&mut ctx, SA0, 3).unwrap(), None);

    // transferFromPerp
    wrap(&mut ctx, transferFromPerpCall { amount: 100_000_000 }.abi_encode());
    assert_eq!((spot(&mut ctx, SA0), perp(&mut ctx, SA0)), (1_100_000_000, 1_350_000_000));

    // Through all of it, the master's blobs never moved.
    assert!(owned(&mut ctx, MA) == ma, "an action AS the sub landed on the master");
    assert_eq!(position(&mut ctx, MA), PerpPosition::default());

    // internalTransfer as the sub → its master.
    wrap(&mut ctx, xfer(MA, SPOT, SPOT, 100_000_000));
    assert_eq!(spot(&mut ctx, MA), 7_100 * ONE_USDC);
    assert_eq!(spot(&mut ctx, SA0), 1_000 * ONE_USDC);

    // §6: existing views work on a sub unchanged.
    let a = getAccountCall::abi_decode_returns(
        &view(&mut ctx, &getAccountCall { user: SA0 }.abi_encode()).bytes,
    )
    .unwrap();
    assert_eq!(a.usdcBalance, U256::from(1_000 * ONE_USDC));
    assert_eq!(a.availableBalance, 1_350_000_000, "no resting orders: available == wallet");
}

/// §3 / §9 "gas charged = inner + overhead; insufficient gas → clean OutOfGas, zero writes",
/// for every inner shape incl. both batch selectors' dynamic cost.
#[test]
fn as_sub_account_bills_inner_plus_overhead_and_one_gas_short_is_a_clean_out_of_gas() {
    assert_eq!(crate::account::sub_account::AS_SUB_ACCOUNT_OVERHEAD_GAS, OVERHEAD);
    let mut ctx = ctx();
    seed(&mut ctx, MA, 5_000 * ONE_USDC, 0);
    assert_eq!(create(&mut ctx, MA, "desk"), SA0);
    ok(&mut ctx, MA, &xfer(SA0, SPOT, SPOT, 3_000 * ONE_USDC));
    let sk = SigningKey::from_bytes(&[0x61; 32]);
    let item = |price| PlaceItem {
        marketId: MARKET_ID,
        side: BUY,
        price,
        quantity: 1,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    };
    // placeOrder (nonce 0) + 3 batch items (1..=3) → the conditional takes nonce 4.
    let algo_id_4 = keccak256([SA0.as_slice(), &4u64.to_be_bytes()[..]].concat()).0;
    let steps: Vec<(&str, Vec<u8>, u64)> = vec![
        ("transferToPerp", transferToPerpCall { amount: 1_000 * ONE_USDC }.abi_encode(), 30_000),
        ("setLeverage", setLeverageCall { marketId: MARKET_ID, leverage: 2 }.abi_encode(), 30_000),
        ("placeOrder", order(BUY, 9_000, 1), 200_000),
        (
            "batchPlaceOrders ×3",
            batchPlaceOrdersCall { orders: vec![item(8_900), item(8_800), item(8_700)] }.abi_encode(),
            20_000 + 3 * 200_000,
        ),
        (
            "batchCancelOrders ×2",
            batchCancelOrdersCall { orderIds: vec![OID_SA0_1.into(), OID_SA0_2.into()] }.abi_encode(),
            20_000 + 2 * 80_000,
        ),
        ("cancelOrder", cancel(OID_SA0_0), 80_000),
        ("registerApiKey", register_key(&sk, 0), 30_000),
        ("revokeApiKey", revokeApiKeyCall { keyId: 0 }.abi_encode(), 20_000),
        ("placeAlgoOrder", stop_sell(8_000, 1), 200_000),
        (
            "cancelAlgoOrder",
            cancelAlgoOrderCall { algoId: algo_id_4.into(), marketId: MARKET_ID }.abi_encode(),
            80_000,
        ),
        ("internalTransfer", xfer(MA, SPOT, SPOT, 1), 50_000),
        ("transferFromPerp", transferFromPerpCall { amount: 1 }.abi_encode(), 30_000),
    ];
    for (name, inner, inner_gas) in steps {
        let input = as_sub(SA0, inner);
        let full = inner_gas + OVERHEAD;
        out_of_gas(&mut ctx, MA, &input, full - 1, name);
        let (out, logs) = ok_at(&mut ctx, MA, &input, full);
        assert_eq!(out.gas_used, full, "[{name}]");
        match name {
            "batchPlaceOrders ×3" => {
                let blob = batchPlaceOrdersCall::abi_decode_returns(&out.bytes).unwrap();
                assert_eq!(
                    batch_records(&blob),
                    vec![(1, OID_SA0_1, 0), (1, OID_SA0_2, 0), (1, OID_SA0_3, 0)],
                    "three Accepted placements under the SUB's ids"
                );
                let owners: Vec<_> =
                    decode_events::<OrderPlaced>(&logs).iter().map(|e| e.user).collect();
                assert_eq!(owners, vec![SA0, SA0, SA0]);
            }
            "batchCancelOrders ×2" => {
                let blob = batchCancelOrdersCall::abi_decode_returns(&out.bytes).unwrap();
                assert_eq!(batch_records(&blob), vec![(1, OID_SA0_1, 0), (1, OID_SA0_2, 0)]);
            }
            "placeAlgoOrder" => assert_eq!(&out.bytes[..32], &algo_id_4),
            _ => {}
        }
    }
    // Each step landed exactly once.
    assert_eq!((nonce(&mut ctx, SA0), nonce(&mut ctx, MA)), (5, 0));
    assert!(storage::load_order(&mut ctx, &OID_SA0_0).unwrap().is_none());
    assert!(storage::load_order(&mut ctx, &OID_SA0_1).unwrap().is_none());
    assert!(storage::load_order(&mut ctx, &OID_SA0_2).unwrap().is_none());
    assert_eq!(
        Address::from(storage::load_order(&mut ctx, &OID_SA0_3).unwrap().unwrap().owner),
        SA0
    );
    assert!(storage::load_user_algo_orders(&mut ctx, SA0, MARKET_ID).unwrap().is_empty());
    assert_eq!(storage::load_api_key(&mut ctx, SA0, 0).unwrap(), None);
    assert_eq!(
        (spot(&mut ctx, SA0), perp(&mut ctx, SA0)),
        (2_000 * ONE_USDC - 1 + 1, 1_000 * ONE_USDC as i64 - 1)
    );
    assert_eq!(spot(&mut ctx, MA), 2_000 * ONE_USDC + 1);
    // Below the wrapper's own floor nothing is even decoded.
    out_of_gas(&mut ctx, MA, &as_sub(SA0, transferToPerpCall { amount: 1 }.abi_encode()), OVERHEAD - 1, "floor");
}

/// §9 "two subs placing direct orders with identical nonces produce DIFFERENT order ids (no
/// overwrite)" — and the master too; plus per-address cancel ownership inside one family.
#[test]
fn two_subs_at_the_same_nonce_get_distinct_order_ids_and_neither_overwrites() {
    let mut ctx = ctx();
    assert_eq!(create(&mut ctx, MA, "a"), SA0);
    assert_eq!(create(&mut ctx, MA, "b"), SA1);
    for u in [MA, SA0, SA1] {
        seed(&mut ctx, u, 0, 1_000 * ONE_USDC as i64);
    }
    for (who, wrapped, expected) in
        [(MA, false, OID_MA_0), (SA0, true, OID_SA0_0), (SA1, true, OID_SA1_0)]
    {
        assert_eq!(nonce(&mut ctx, who), 0, "{who}: every member starts at nonce 0");
        let input = if wrapped { as_sub(who, order(BUY, 9_000, 1)) } else { order(BUY, 9_000, 1) };
        let (out, logs) = ok(&mut ctx, MA, &input);
        assert_eq!(order_id_of(&out), expected, "{who}");
        let placed = decode_events::<OrderPlaced>(&logs);
        assert_eq!((placed[0].user, placed[0].orderId.0), (who, expected));
        assert_eq!(nonce(&mut ctx, who), 1);
    }
    assert_eq!(
        BTreeSet::from([OID_MA_0, OID_SA0_0, OID_SA1_0]).len(),
        3,
        "same nonce, three different ids"
    );
    // All three live side by side — nothing overwrote anything.
    for (who, id) in [(MA, OID_MA_0), (SA0, OID_SA0_0), (SA1, OID_SA1_0)] {
        let o = storage::load_order(&mut ctx, &id).unwrap().unwrap();
        assert_eq!((Address::from(o.owner), o.price, o.quantity, o.filled), (who, 9_000, 1, 0));
    }
    assert_eq!(storage::load_bid_count(&mut ctx, MARKET_ID, 9_000).unwrap(), 3);
    let open = getOpenOrdersCall::abi_decode_returns(
        &view(&mut ctx, &getOpenOrdersCall { user: SA0, marketId: MARKET_ID }.abi_encode())
            .bytes,
    )
    .unwrap();
    assert_eq!(open.orderIds.iter().map(|b| b.0).collect::<Vec<_>>(), vec![OID_SA0_0]);

    // Ownership is per ADDRESS: no family member can cancel another's order.
    rejected(&mut ctx, MA, &as_sub(SA0, cancel(OID_MA_0)), E_NOT_OWNER);
    rejected(&mut ctx, MA, &cancel(OID_SA0_0), E_NOT_OWNER);
    rejected(&mut ctx, MA, &as_sub(SA1, cancel(OID_SA0_0)), E_NOT_OWNER);
    ok(&mut ctx, MA, &as_sub(SA1, cancel(OID_SA1_0)));
    assert!(storage::load_order(&mut ctx, &OID_SA1_0).unwrap().is_none());
    assert!(storage::load_order(&mut ctx, &OID_SA0_0).unwrap().is_some());
    assert!(storage::load_order(&mut ctx, &OID_MA_0).unwrap().is_some());
    assert_eq!(storage::load_bid_count(&mut ctx, MARKET_ID, 9_000).unwrap(), 2);
}

/// §9 "isolation: liquidating a sub leaves the master and siblings untouched".
///
/// SA0 is a 5× long (margin 200M) — at a $90 mark its equity is 100M < MM 150M. SA1 and MA are
/// 2× longs (margin 500M, equity 400M) and stay healthy; both also have a resting bid below the
/// $90 level the liquidation closes into. MAKER's bid absorbs all 10, so there is no ADL leg.
#[test]
fn liquidating_a_sub_leaves_its_master_and_siblings_untouched() {
    let mut ctx = ctx();
    assert_eq!(create(&mut ctx, MA, "a"), SA0);
    assert_eq!(create(&mut ctx, MA, "b"), SA1);
    seed(&mut ctx, SA0, 0, 50_000_000);
    seed_position(&mut ctx, SA0, 10, -1_000_000_000, 200_000_000, 5);
    seed(&mut ctx, SA1, 0, 300_000_000);
    seed_position(&mut ctx, SA1, 10, -1_000_000_000, 500_000_000, 2);
    seed(&mut ctx, MA, 0, 400_000_000);
    seed_position(&mut ctx, MA, 10, -1_000_000_000, 500_000_000, 2);
    let (out, _) = ok(&mut ctx, MA, &as_sub(SA1, order(BUY, 8_000, 1)));
    assert_eq!(order_id_of(&out), OID_SA1_0);
    let (out, _) = ok(&mut ctx, MA, &order(BUY, 7_000, 1));
    assert_eq!(order_id_of(&out), OID_MA_0);
    storage::save_mark_price(&mut ctx, MARKET_ID, 9_000).unwrap();
    place_maker_order(&mut ctx, BUY, 9_000, 10);

    let ma_before = owned(&mut ctx, MA);
    let sa1_before = owned(&mut ctx, SA1);
    let orders_before = (
        perp_core::codec::encode(&storage::load_order(&mut ctx, &OID_MA_0).unwrap()).unwrap(),
        perp_core::codec::encode(&storage::load_order(&mut ctx, &OID_SA1_0).unwrap()).unwrap(),
    );

    let (out, logs) = ok(
        &mut ctx,
        KEEPER,
        &liquidateCall { user: SA0, marketId: MARKET_ID }.abi_encode(),
    );
    assert_eq!(out.gas_used, 150_000);

    // SA0: flat, and its wallet got back margin − loss = 200M − 100M (no fees anywhere).
    let p = position(&mut ctx, SA0);
    assert_eq!((p.amount, p.v_quote_balance, p.margin), (0, 0, 0));
    assert_eq!(perp(&mut ctx, SA0), 150_000_000);
    let liq = decode_events::<Liquidation>(&logs);
    assert_eq!((liq.len(), liq[0].user), (1, SA0));
    assert_eq!(position(&mut ctx, MAKER).amount, 10);

    // The master and the sibling: bit-identical, orders included.
    assert!(owned(&mut ctx, MA) == ma_before, "liquidating a sub touched its master");
    assert!(owned(&mut ctx, SA1) == sa1_before, "liquidating a sub touched its sibling");
    assert_eq!(
        (
            perp_core::codec::encode(&storage::load_order(&mut ctx, &OID_MA_0).unwrap()).unwrap(),
            perp_core::codec::encode(&storage::load_order(&mut ctx, &OID_SA1_0).unwrap()).unwrap(),
        ),
        orders_before
    );
    assert_eq!(storage::load_bid_count(&mut ctx, MARKET_ID, 8_000).unwrap(), 1);
    assert_eq!(storage::load_bid_count(&mut ctx, MARKET_ID, 7_000).unwrap(), 1);
    // …and no event names either of them.
    let named: BTreeSet<Address> = decode_events::<AccountBalanceChanged>(&logs)
        .iter()
        .map(|e| e.user)
        .chain(decode_events::<PositionChanged>(&logs).iter().map(|e| e.user))
        .collect();
    assert!(!named.contains(&MA) && !named.contains(&SA1), "{named:?}");
}

/// §9 "API key registered via asSubAccount on a sub; a signed order by that key for
/// account=sub works; the same key cannot sign for the master" — nor for a sibling, and there
/// is no master-key fallback (decision 3). A VALID signed call is still denied by the wrapper.
#[test]
fn a_sub_s_api_key_signs_for_that_sub_only() {
    let mut ctx = ctx();
    assert_eq!(create(&mut ctx, MA, "a"), SA0);
    assert_eq!(create(&mut ctx, MA, "b"), SA1);
    for u in [MA, SA0, SA1] {
        seed(&mut ctx, u, 0, 1_000 * ONE_USDC as i64);
    }
    let k_sa0 = SigningKey::from_bytes(&[0x51; 32]);
    let k_sa1 = SigningKey::from_bytes(&[0x52; 32]);
    let k_ma = SigningKey::from_bytes(&[0x53; 32]);

    let (_, logs) = ok(&mut ctx, MA, &as_sub(SA0, register_key(&k_sa0, 0)));
    let reg = decode_events::<ApiKeyRegistered>(&logs);
    assert_eq!((reg.len(), reg[0].user, reg[0].keyId), (1, SA0, 0));
    assert_eq!(
        storage::load_api_key(&mut ctx, SA0, 0).unwrap(),
        Some(ApiKey { pubkey: k_sa0.verifying_key().to_bytes(), expiry: 0 })
    );
    assert_eq!(storage::load_api_key(&mut ctx, MA, 0).unwrap(), None);
    assert_eq!(storage::load_api_key(&mut ctx, SA1, 0).unwrap(), None);
    ok(&mut ctx, MA, &register_key(&k_ma, 0)); // the master's OWN key, in its own slot 0

    // A valid signed order for SA0 — but a *Signed selector is denied inside the wrapper.
    let (input, sig) = signed_buy(&mut ctx, &k_sa0, SA0, 9_000, 0);
    rejected(&mut ctx, MA, &as_sub(SA0, input.clone()), E_DENIED);
    // Relayed directly (by anyone), it works, and the order is the SUB's.
    let (out, logs) = ok(&mut ctx, X, &input);
    let id = placeOrderSignedCall::abi_decode_returns(&out.bytes).unwrap().0;
    assert_eq!(id, keccak256(sig).0, "signed-path id = keccak256(signature)");
    let o = storage::load_order(&mut ctx, &id).unwrap().unwrap();
    assert_eq!((Address::from(o.owner), o.price), (SA0, 9_000));
    assert_eq!(decode_events::<OrderPlaced>(&logs)[0].user, SA0);

    // SA0's key for the MASTER (whose slot 0 holds the master's own key): refused.
    let (input, _) = signed_buy(&mut ctx, &k_sa0, MA, 8_900, 0);
    rejected(&mut ctx, X, &input, E_SIG_BAD);
    // …for a SIBLING with no key: refused.
    let (input, _) = signed_buy(&mut ctx, &k_sa0, SA1, 8_800, 0);
    rejected(&mut ctx, X, &input, E_SIG_NO_KEY);
    // …for a sibling WITH its own key: still refused; the sibling's own key works.
    ok(&mut ctx, MA, &as_sub(SA1, register_key(&k_sa1, 0)));
    let (input, _) = signed_buy(&mut ctx, &k_sa0, SA1, 8_700, 0);
    rejected(&mut ctx, X, &input, E_SIG_BAD);
    let (input, _) = signed_buy(&mut ctx, &k_sa1, SA1, 8_700, 0);
    ok(&mut ctx, X, &input);
    // No master-key fallback: the master's key cannot sign for its sub.
    let (input, _) = signed_buy(&mut ctx, &k_ma, SA0, 8_600, 0);
    rejected(&mut ctx, X, &input, E_SIG_BAD);

    // Revoked through the wrapper, the sub's key is dead.
    ok(&mut ctx, MA, &as_sub(SA0, revokeApiKeyCall { keyId: 0 }.abi_encode()));
    let (input, _) = signed_buy(&mut ctx, &k_sa0, SA0, 8_500, 0);
    rejected(&mut ctx, X, &input, E_SIG_NO_KEY);
    assert!(storage::load_api_key(&mut ctx, MA, 0).unwrap().is_some(), "the master's key stays");
}

// ═════════════════════════════════════════════════════════════════════════════════════════
// §5 internalTransfer
// ═════════════════════════════════════════════════════════════════════════════════════════

/// §5 / §9 "family (all 4 wallet combos, master↔sub, sub↔sub via asSubAccount)", `from == to`
/// with different wallets, conservation, and the event stream: `InternalTransfer`, then one
/// `AccountBalanceChanged(reason 8)` per account in ASCENDING address order, nothing else.
#[test]
fn internal_transfer_moves_every_wallet_combination_inside_a_family() {
    let mut ctx = ctx();
    assert_eq!(create(&mut ctx, MA, "a"), SA0);
    assert_eq!(create(&mut ctx, MA, "b"), SA1);
    for u in [MA, SA0, SA1] {
        seed(&mut ctx, u, 1_000 * ONE_USDC, 1_000 * ONE_USDC as i64);
    }
    let family = [MA, SA0, SA1];
    let total = money(&mut ctx, &family);
    assert_eq!(total, 6_000 * ONE_USDC as i128);
    // 7 quote units: far below the external 1-USDC floor, which a family move does not have.
    const AMT: u64 = 7;
    let wallets = |ctx: &mut TestCtx, u| (spot(ctx, u), perp(ctx, u));
    for (from, to, wrapped) in [
        (MA, SA0, false),
        (SA0, MA, true),
        (SA0, SA1, true),
        (SA1, SA0, true),
        (MA, SA1, false),
        (SA1, MA, true),
    ] {
        for (fw, tw) in [(SPOT, SPOT), (SPOT, PERP), (PERP, SPOT), (PERP, PERP)] {
            let label = format!("{from} w{fw} → {to} w{tw}");
            let (mut f, mut t) = (wallets(&mut ctx, from), wallets(&mut ctx, to));
            if fw == SPOT { f.0 -= AMT } else { f.1 -= AMT as i64 }
            if tw == SPOT { t.0 += AMT } else { t.1 += AMT as i64 }
            let inner = xfer(to, fw, tw, AMT);
            let (input, gas) =
                if wrapped { (as_sub(from, inner), 50_000 + OVERHEAD) } else { (inner, 50_000) };
            let (out, logs) = ok(&mut ctx, MA, &input);
            assert_eq!(out.gas_used, gas, "{label}");
            assert!(out.bytes.is_empty(), "{label}");
            assert_eq!(wallets(&mut ctx, from), f, "{label}: source");
            assert_eq!(wallets(&mut ctx, to), t, "{label}: destination");
            assert_eq!(money(&mut ctx, &family), total, "{label}: Σ spot + Σ perp moved");

            assert_eq!(logs.len(), 3, "{label}");
            let it = decode_events::<InternalTransfer>(&logs[..1]);
            assert_eq!(it.len(), 1, "{label}: InternalTransfer comes FIRST");
            assert_eq!((it[0].from, it[0].to, it[0].fromWallet, it[0].toWallet, it[0].amount), (from, to, fw, tw, AMT));
            let ((lo, lw), (hi, hw)) = if from < to { ((from, f), (to, t)) } else { ((to, t), (from, f)) };
            assert_eq!(
                balance_rows(&logs),
                vec![
                    (lo, 8, U256::from(lw.0), lw.1, lw.1),
                    (hi, 8, U256::from(hw.0), hw.1, hw.1),
                ],
                "{label}: both after-images, reason 8, ascending address"
            );
            assert_account_update_groups(&logs);
        }
    }

    // `from == to`, different wallets: one account, one row (a sub through the wrapper, the
    // master directly).
    for (who, wrapped) in [(SA0, true), (MA, false)] {
        for (fw, tw) in [(SPOT, PERP), (PERP, SPOT)] {
            let mut w = wallets(&mut ctx, who);
            if fw == SPOT { w.0 -= AMT; w.1 += AMT as i64 } else { w.0 += AMT; w.1 -= AMT as i64 }
            let inner = xfer(who, fw, tw, AMT);
            let input = if wrapped { as_sub(who, inner) } else { inner };
            let (_, logs) = ok(&mut ctx, MA, &input);
            assert_eq!(wallets(&mut ctx, who), w);
            assert_eq!(logs.len(), 2);
            assert_eq!(balance_rows(&logs), vec![(who, 8, U256::from(w.0), w.1, w.1)]);
        }
    }
    assert_eq!(money(&mut ctx, &family), total);
    // No ERC-20 moved: the custody contract balance of the DEX is untouched (still 0).
    assert_eq!(storage::load_erc20_balance(&mut ctx, USDC_ADDRESS, PERP_DEX_ADDRESS).unwrap(), U256::ZERO);
}

/// §5 / §9 external rules + every common reject, each write-free, then the accepted shapes at
/// their exact boundaries.
#[test]
fn internal_transfer_external_rules_and_every_reject() {
    let mut ctx = ctx();
    assert_eq!(create(&mut ctx, MA, "a"), SA0);
    assert_eq!(create(&mut ctx, MB, "b"), SB0);
    const BAL: u64 = 10 * ONE_USDC;
    for u in [MA, MB, X, SA0, SB0] {
        seed(&mut ctx, u, BAL, BAL as i64);
    }
    let everyone = [MA, MB, X, SA0, SB0];
    let total = money(&mut ctx, &everyone);
    let wrap_sa0 = |inner| as_sub(SA0, inner);
    for (caller, input, expect) in [
        // ── external: SPOT → SPOT only, ≥ 1 USDC ──
        (MA, xfer(X, SPOT, SPOT, ONE_USDC - 1), E_XFER_EXT_MIN),
        (MA, xfer(MB, SPOT, SPOT, ONE_USDC - 1), E_XFER_EXT_MIN),
        (MA, xfer(X, PERP, SPOT, ONE_USDC), E_XFER_EXT_WALLETS),
        (MA, xfer(X, SPOT, PERP, ONE_USDC), E_XFER_EXT_WALLETS),
        (MA, xfer(X, PERP, PERP, ONE_USDC), E_XFER_EXT_WALLETS),
        (MA, xfer(MB, PERP, PERP, ONE_USDC), E_XFER_EXT_WALLETS),
        // ── external from a SUB: refused whatever the recipient ──
        (MA, wrap_sa0(xfer(X, SPOT, SPOT, ONE_USDC)), E_XFER_FROM_SUB),
        (MA, wrap_sa0(xfer(MB, SPOT, SPOT, ONE_USDC)), E_XFER_FROM_SUB),
        (MA, wrap_sa0(xfer(SB0, SPOT, SPOT, ONE_USDC)), E_XFER_FROM_SUB),
        // an uncreated derived address of its own master is NOT family
        (MA, wrap_sa0(xfer(SA10, SPOT, SPOT, ONE_USDC)), E_XFER_FROM_SUB),
        // ── external to ANOTHER family's sub ──
        (MA, xfer(SB0, SPOT, SPOT, ONE_USDC), E_XFER_TO_FOREIGN_SUB),
        (X, xfer(SA0, SPOT, SPOT, ONE_USDC), E_XFER_TO_FOREIGN_SUB),
        (MB, xfer(SA0, SPOT, SPOT, ONE_USDC), E_XFER_TO_FOREIGN_SUB),
        // ── recipient ──
        (MA, xfer(Address::ZERO, SPOT, SPOT, ONE_USDC), E_XFER_TO_ZERO),
        (MA, xfer(PERP_DEX_ADDRESS, SPOT, SPOT, ONE_USDC), E_XFER_TO_DEX),
        (MA, wrap_sa0(xfer(Address::ZERO, SPOT, SPOT, ONE_USDC)), E_XFER_TO_ZERO),
        (MA, wrap_sa0(xfer(PERP_DEX_ADDRESS, PERP, PERP, 1)), E_XFER_TO_DEX),
        // ── amount ──
        (MA, xfer(X, SPOT, SPOT, 0), E_XFER_ZERO_AMOUNT),
        (MA, xfer(SA0, PERP, PERP, 0), E_XFER_ZERO_AMOUNT),
        // ── wallet codes ──
        (MA, xfer(SA0, 2, SPOT, 1), E_XFER_WALLET),
        (MA, xfer(SA0, SPOT, 2, 1), E_XFER_WALLET),
        (MA, xfer(MA, 255, 255, 1), E_XFER_WALLET),
        // ── same wallet ──
        (MA, xfer(MA, SPOT, SPOT, 1), E_XFER_SAME),
        (MA, xfer(MA, PERP, PERP, 1), E_XFER_SAME),
        (MA, wrap_sa0(xfer(SA0, SPOT, SPOT, 1)), E_XFER_SAME),
        // ── source balance ──
        (MA, xfer(X, SPOT, SPOT, BAL + 1), E_XFER_SPOT),
        (MA, xfer(SA0, SPOT, PERP, BAL + 1), E_XFER_SPOT),
        (MA, wrap_sa0(xfer(MA, SPOT, SPOT, BAL + 1)), E_XFER_SPOT),
        (MA, xfer(SA0, PERP, SPOT, BAL + 1), E_XFER_PERP),
        (MA, wrap_sa0(xfer(MA, PERP, PERP, BAL + 1)), E_XFER_PERP),
    ] {
        rejected(&mut ctx, caller, &input, expect);
    }
    assert_eq!(money(&mut ctx, &everyone), total);

    // ── accepted: external SPOT→SPOT at exactly 1 USDC ──
    let (out, logs) = ok(&mut ctx, MA, &xfer(X, SPOT, SPOT, ONE_USDC));
    assert_eq!(out.gas_used, 50_000);
    assert_eq!((spot(&mut ctx, MA), spot(&mut ctx, X)), (BAL - ONE_USDC, BAL + ONE_USDC));
    let it = decode_events::<InternalTransfer>(&logs);
    assert_eq!((it[0].from, it[0].to, it[0].fromWallet, it[0].toWallet, it[0].amount), (MA, X, SPOT, SPOT, ONE_USDC));
    assert!(MA < X);
    assert_eq!(
        balance_rows(&logs),
        vec![
            (MA, 8, U256::from(BAL - ONE_USDC), BAL as i64, BAL as i64),
            (X, 8, U256::from(BAL + ONE_USDC), BAL as i64, BAL as i64),
        ]
    );
    assert_eq!(account_update_reasons(&logs), vec![(MA, R::InternalTransfer), (X, R::InternalTransfer)]);
    // ordinary → master, master → master
    ok(&mut ctx, X, &xfer(MA, SPOT, SPOT, ONE_USDC));
    ok(&mut ctx, MA, &xfer(MB, SPOT, SPOT, ONE_USDC));
    assert_eq!((spot(&mut ctx, MA), spot(&mut ctx, MB), spot(&mut ctx, X)), (BAL - ONE_USDC, BAL + ONE_USDC, BAL));
    // the whole spot balance (≥ the minimum) can leave
    ok(&mut ctx, MB, &xfer(X, SPOT, SPOT, BAL + ONE_USDC));
    assert_eq!((spot(&mut ctx, MB), spot(&mut ctx, X)), (0, 2 * BAL + ONE_USDC));
    // `from == to` is the account's own family: below the minimum is fine
    let (_, logs) = ok(&mut ctx, X, &xfer(X, SPOT, PERP, 5));
    assert_eq!(balance_rows(&logs), vec![(X, 8, U256::from(2 * BAL + ONE_USDC - 5), BAL as i64 + 5, BAL as i64 + 5)]);
    assert_eq!(money(&mut ctx, &everyone), total, "Σ spot + Σ perp over every account");
}

/// §5 / §9 "PERP source gated by derived available incl. after a mark move that changes ooIM".
///
/// SA0 (2×) is long 10 @ $100 (margin 500M, wallet 1.5B) with a resting SELL 30 @ $110, so
/// `Ask = 30 × 11_000 × 1e4 = 3.3B` and `ooIM = ⌈max(|N|, |N − Ask|)/2⌉ − ⌈|N|/2⌉`:
///
/// * mark $100, N = 1.0B: `⌈2.3B/2⌉ − ⌈1.0B/2⌉ = 1.15B − 0.50B = 650M` → available 850M
/// * mark  $90, N = 0.9B: `⌈2.4B/2⌉ − ⌈0.9B/2⌉ = 1.20B − 0.45B = 750M` → available 750M
///
/// So the mark move ALONE (no action by SA0) shrinks what may leave the perp wallet by 100M.
#[test]
fn a_perp_source_is_gated_on_derived_available_and_a_mark_move_moves_the_gate() {
    let mut ctx = ctx();
    assert_eq!(create(&mut ctx, MA, "a"), SA0);
    assert_eq!(create(&mut ctx, MA, "b"), SA1);
    seed(&mut ctx, MA, 2_000 * ONE_USDC, 0);
    ok(&mut ctx, MA, &xfer(SA0, SPOT, PERP, 2_000 * ONE_USDC));
    ok(&mut ctx, MA, &as_sub(SA0, setLeverageCall { marketId: MARKET_ID, leverage: 2 }.abi_encode()));
    place_maker_order(&mut ctx, SELL, ENTRY_PRICE, 10);
    ok(&mut ctx, MA, &as_sub(SA0, order(BUY, ENTRY_PRICE, 10)));
    let p = position(&mut ctx, SA0);
    assert_eq!((p.amount, p.v_quote_balance, p.margin, p.leverage), (10, -1_000_000_000, 500_000_000, 2));
    assert_eq!(perp(&mut ctx, SA0), 1_500_000_000);
    ok(&mut ctx, MA, &as_sub(SA0, order(SELL, 11_000, 30)));
    assert_eq!(position(&mut ctx, SA0).total_sell_notional, 3_300_000_000);

    // ── at $100 ──
    assert_eq!(oo_im(&mut ctx, SA0), 650_000_000);
    assert_eq!(available(&mut ctx, SA0), 850_000_000);
    assert_eq!(crate::margin_view::derived_available_balance(&mut ctx, SA0).unwrap(), 850_000_000);
    rejected(&mut ctx, MA, &as_sub(SA0, xfer(MA, PERP, SPOT, 850_000_001)), E_XFER_PERP);

    // ── the mark moves; SA0 does nothing ──
    storage::save_mark_price(&mut ctx, MARKET_ID, 9_000).unwrap();
    assert_eq!(oo_im(&mut ctx, SA0), 750_000_000);
    assert_eq!(available(&mut ctx, SA0), 750_000_000);
    assert_eq!(perp(&mut ctx, SA0), 1_500_000_000, "the wallet itself did not move");
    // What was affordable a moment ago no longer is — through EVERY perp-source shape.
    rejected(&mut ctx, MA, &as_sub(SA0, xfer(MA, PERP, SPOT, 850_000_000)), E_XFER_PERP);
    rejected(&mut ctx, MA, &as_sub(SA0, xfer(MA, PERP, SPOT, 750_000_001)), E_XFER_PERP);
    rejected(&mut ctx, MA, &as_sub(SA0, xfer(MA, PERP, PERP, 750_000_001)), E_XFER_PERP);
    rejected(&mut ctx, MA, &as_sub(SA0, xfer(SA1, PERP, PERP, 750_000_001)), E_XFER_PERP);
    rejected(&mut ctx, MA, &as_sub(SA0, xfer(SA0, PERP, SPOT, 750_000_001)), E_XFER_PERP);
    // …and at the SAME boundary as transferFromPerp (spec: "REUSE the exact gate").
    rejected(&mut ctx, MA, &as_sub(SA0, transferFromPerpCall { amount: 750_000_001 }.abi_encode()), E_FROM_PERP);

    // Exactly the new available: accepted.
    let total = money(&mut ctx, &[MA, SA0, SA1]);
    ok(&mut ctx, MA, &as_sub(SA0, xfer(MA, PERP, SPOT, 750_000_000)));
    assert_eq!(perp(&mut ctx, SA0), 750_000_000);
    assert_eq!(spot(&mut ctx, MA), 750_000_000);
    assert_eq!(available(&mut ctx, SA0), 0);
    assert_eq!(money(&mut ctx, &[MA, SA0, SA1]), total);
    rejected(&mut ctx, MA, &as_sub(SA0, xfer(SA1, PERP, SPOT, 1)), E_XFER_PERP);
    // A SPOT source is not gated by ooIM (SA0 has no spot here, so: the spot check).
    rejected(&mut ctx, MA, &as_sub(SA0, xfer(MA, SPOT, SPOT, 1)), E_XFER_SPOT);
}

// ═════════════════════════════════════════════════════════════════════════════════════════
// Decision 4: no deposit / withdraw for a sub
// ═════════════════════════════════════════════════════════════════════════════════════════

/// §9 "deposit/withdraw via asSubAccount rejected" — with VALID calldata, a sub holding both an
/// internal spot balance AND real ERC-20 USDC at its derived address, so a bypass would move
/// money. Then the sanctioned route out: sub → master by `internalTransfer`, master withdraws.
#[test]
fn deposit_and_withdraw_through_the_wrapper_are_refused_and_money_leaves_via_the_master() {
    let mut db = InMemoryDB::default();
    for (who, amount) in [(MA, 5_000 * ONE_USDC), (SA0, 1_000 * ONE_USDC)] {
        db.insert_account_storage(USDC_ADDRESS, erc20_balance_slot(who).into(), U256::from(amount))
            .unwrap();
    }
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for a in [USDC_ADDRESS, PERP_DEX_ADDRESS, MA, SA0, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), a).unwrap();
    }
    setup_market(&mut ctx);
    let erc20 = |ctx: &mut TestCtx, who| {
        storage::load_erc20_balance(ctx, USDC_ADDRESS, who).unwrap().to::<u64>()
    };

    ok(&mut ctx, MA, &depositCall { amount: U256::from(3_000 * ONE_USDC) }.abi_encode());
    assert_eq!(create(&mut ctx, MA, "desk"), SA0);
    ok(&mut ctx, MA, &xfer(SA0, SPOT, SPOT, 2_000 * ONE_USDC));
    let ledger = |ctx: &mut TestCtx| {
        (erc20(ctx, MA), erc20(ctx, SA0), erc20(ctx, PERP_DEX_ADDRESS), spot(ctx, MA), spot(ctx, SA0))
    };
    let before = ledger(&mut ctx);
    assert_eq!(
        before,
        (2_000 * ONE_USDC, 1_000 * ONE_USDC, 3_000 * ONE_USDC, 1_000 * ONE_USDC, 2_000 * ONE_USDC)
    );

    let out = rejected(&mut ctx, MA, &as_sub(SA0, withdrawCall { amount: U256::from(500 * ONE_USDC) }.abi_encode()), E_DENIED_DW);
    assert_eq!(out.gas_used, OVERHEAD);
    let out = rejected(&mut ctx, MA, &as_sub(SA0, depositCall { amount: U256::from(500 * ONE_USDC) }.abi_encode()), E_DENIED_DW);
    assert_eq!(out.gas_used, OVERHEAD);
    assert_eq!(ledger(&mut ctx), before, "no ERC-20 or internal balance moved");

    // The sanctioned route: the sub's money leaves through its master.
    ok(&mut ctx, MA, &as_sub(SA0, xfer(MA, SPOT, SPOT, 500 * ONE_USDC)));
    ok(&mut ctx, MA, &withdrawCall { amount: U256::from(1_500 * ONE_USDC) }.abi_encode());
    assert_eq!(
        ledger(&mut ctx),
        (3_500 * ONE_USDC, 1_000 * ONE_USDC, 1_500 * ONE_USDC, 0, 1_500 * ONE_USDC)
    );
}
