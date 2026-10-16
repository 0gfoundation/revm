//! Sub-accounts (P1): `createSubAccount`, `asSubAccount`, `internalTransfer`, the two views and the
//! `setUserFeeRates` push-down — every call driven through the full shell (`run_perp_dex_call`), so
//! the gas charge, the EOA gate, the commit-only #23 tripwire (a `debug_assert` on any
//! write-then-revert) and the end-of-call `AccountBalanceChanged` drain are all in the loop.
use super::*;
use alloy_sol_types::{SolCall, SolError, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, FixedBytes};

use crate::{
    batch::BASE_BATCH_GAS,
    call::selectors_map,
    events::stream_test_support::{account_update_reasons, assert_account_update_groups},
    interface::IPerpDex::{
        getAccountCall, getApiKeyCall, getUserFeeRatesCall, setUserFeeRatesCall,
        transferToPerpSignedCall, InternalTransfer, SubAccountCreated, UserFeeRatesUpdated,
    },
    run_perp_dex_call,
    types::{ApiKey, MarginTiers, Market},
    PerpOutput, CANCEL_ORDER_GAS, PLACE_ORDER_GAS, USDC_ADDRESS,
};

const MASTER: Address = address!("1111111111111111111111111111111111111111");
/// A second, unrelated master / ordinary user.
const OTHER: Address = address!("2222222222222222222222222222222222222222");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

const MARKET_ID: u64 = 1;
/// 1 tick = $1 at price_decimals = 9.
const TICK: u64 = 1_000_000_000;
const PRICE: u64 = 100 * TICK;
/// 0.01 BTC at base_decimals = 8; `calc_value(PRICE, QTY)` = 1 USDC.
const QTY: u64 = 1_000_000;
const USDC: u64 = 1_000_000;

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

// ── Fixture ───────────────────────────────────────────────────────────────────

fn make_ctx() -> TestCtx {
    let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, MASTER, OTHER, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    storage::save_market(
        &mut ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 8,
            price_decimals: 9,
            tick_size: TICK,
            step_size: QTY,
            min_quantity: QTY,
            max_quantity: QTY * 1_000,
            max_price: PRICE * 1_000,
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
    // Fixture writes are not under test: drop their logs and marks.
    let _ = JournalTr::take_logs(ctx.journal_mut());
    ctx
}

/// Seeds `user`'s SPOT and PERP balances directly (the deposit path is not under test here).
fn seed(ctx: &mut TestCtx, user: Address, spot: u64, perp: u64) {
    let mut a = storage::load_account(ctx, user).unwrap();
    a.usdc_balance = U256::from(spot).into();
    a.perp_wallet_balance = perp as i64;
    storage::save_account(ctx, user, a, AccountUpdateReason::Adjustment).unwrap();
    storage::flush_account_snapshots(ctx).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());
}

fn call(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, 10_000_000, caller, U256::ZERO, false, ctx)
        .expect("a perp call must not hard-fail")
}

fn revert_reason(out: &PerpOutput) -> String {
    alloy_sol_types::Revert::abi_decode(&out.bytes)
        .map(|r| r.reason)
        .unwrap_or_else(|_| format!("<undecodable {:?}>", out.bytes))
}

/// A successful call; returns its output (logs stay in the journal for the caller to take).
fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    let out = call(ctx, caller, input);
    assert!(!out.reverted, "call reverted: {}", revert_reason(&out));
    out
}

/// A rejected call: reverts with `expect` in the reason, AND — the commit-only #23 property —
/// wrote nothing and logged nothing. (The shell's `debug_assert` tripwire would also fire on a
/// write-then-revert; this asserts it directly, independent of that guard.)
fn rejects(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let writes = JournalTr::perp_write_count(ctx.journal_mut());
    let out = call(ctx, caller, input);
    assert!(out.reverted, "expected a revert containing {expect:?}, the call succeeded");
    let reason = revert_reason(&out);
    assert!(reason.contains(expect), "reason {reason:?} does not contain {expect:?}");
    assert_eq!(
        JournalTr::perp_write_count(ctx.journal_mut()),
        writes,
        "a rejected call wrote perp state ({reason})"
    );
    assert!(
        JournalTr::take_logs(ctx.journal_mut()).is_empty(),
        "a rejected call logged ({reason})"
    );
}

fn create(ctx: &mut TestCtx, master: Address, name: &str) -> Address {
    let out = ok(
        ctx,
        master,
        &createSubAccountCall {
            name: name.into(),
        }
        .abi_encode(),
    );
    createSubAccountCall::abi_decode_returns(&out.bytes).unwrap()
}

fn as_sub(sub: Address, inner: Vec<u8>) -> Vec<u8> {
    asSubAccountCall {
        subAccount: sub,
        data: inner.into(),
    }
    .abi_encode()
}

fn transfer(to: Address, from_wallet: u8, to_wallet: u8, amount: u64) -> Vec<u8> {
    internalTransferCall {
        to,
        fromWallet: from_wallet,
        toWallet: to_wallet,
        amount,
    }
    .abi_encode()
}

fn place_buy() -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE,
        quantity: QTY,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode()
}

/// `(spot, perp_wallet)` straight off the account blob.
fn balances(ctx: &mut TestCtx, user: Address) -> (u64, i64) {
    let a = storage::load_account(ctx, user).unwrap();
    let spot: U256 = a.usdc_balance.clone().into();
    (spot.to::<u64>(), a.perp_wallet_balance)
}

/// `Σ (spot + perp)` over `users` — the conservation witness for internal transfers.
fn total(ctx: &mut TestCtx, users: &[Address]) -> i128 {
    users
        .iter()
        .map(|&u| {
            let (s, p) = balances(ctx, u);
            s as i128 + p as i128
        })
        .sum()
}

fn get_master(ctx: &mut TestCtx, account: Address) -> Address {
    let out = ok(ctx, OTHER, &getMasterCall { account }.abi_encode());
    getMasterCall::abi_decode_returns(&out.bytes).unwrap()
}

fn get_subs(ctx: &mut TestCtx, master: Address) -> Vec<Address> {
    let out = ok(ctx, OTHER, &getSubAccountsCall { master }.abi_encode());
    getSubAccountsCall::abi_decode_returns(&out.bytes).unwrap()
}

fn fee_rates(ctx: &mut TestCtx, user: Address) -> (u64, u64) {
    let out = ok(ctx, OTHER, &getUserFeeRatesCall { user }.abi_encode());
    let r = getUserFeeRatesCall::abi_decode_returns(&out.bytes).unwrap();
    (r.makerFeeBps, r.takerFeeBps)
}

// ── createSubAccount ──────────────────────────────────────────────────────────

/// Sequential indices, each the documented derivation, the event, both views — and the 11th is
/// refused with nothing written.
#[test]
fn create_assigns_sequential_derived_addresses_up_to_the_cap() {
    let mut ctx = make_ctx();

    let sub0 = create(&mut ctx, MASTER, "alpha");
    assert_eq!(sub0, derive_sub_account(MASTER, 0));
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(logs.len(), 1, "exactly one SubAccountCreated, no balance after-image");
    let e = SubAccountCreated::decode_raw_log(logs[0].data.topics(), &logs[0].data.data).unwrap();
    assert_eq!(
        (e.master, e.subAccount, e.index, e.name.as_str()),
        (MASTER, sub0, 0, "alpha")
    );

    for i in 1..MAX_SUB_ACCOUNTS {
        assert_eq!(create(&mut ctx, MASTER, "s"), derive_sub_account(MASTER, i));
    }
    let expected: Vec<Address> = (0..MAX_SUB_ACCOUNTS)
        .map(|i| derive_sub_account(MASTER, i))
        .collect();
    assert_eq!(get_subs(&mut ctx, MASTER), expected);
    for &sub in &expected {
        assert_eq!(get_master(&mut ctx, sub), MASTER);
    }
    assert_eq!(get_master(&mut ctx, MASTER), Address::ZERO, "a master has no master");
    assert!(get_subs(&mut ctx, OTHER).is_empty());

    rejects(
        &mut ctx,
        MASTER,
        &createSubAccountCall { name: "x".into() }.abi_encode(),
        "sub-account limit reached (max 10)",
    );
    assert_eq!(get_subs(&mut ctx, MASTER).len(), MAX_SUB_ACCOUNTS as usize);
}

#[test]
fn a_sub_cannot_create_and_names_are_bounded() {
    let mut ctx = make_ctx();
    let sub = create(&mut ctx, MASTER, "a");

    // Directly (as if it had a key) and through its master's wrapper.
    rejects(
        &mut ctx,
        sub,
        &createSubAccountCall { name: "x".into() }.abi_encode(),
        "a sub-account cannot create sub-accounts",
    );
    rejects(
        &mut ctx,
        MASTER,
        &as_sub(sub, createSubAccountCall { name: "x".into() }.abi_encode()),
        "inner selector is not allowed",
    );

    rejects(
        &mut ctx,
        OTHER,
        &createSubAccountCall { name: String::new() }.abi_encode(),
        "name must not be empty",
    );
    rejects(
        &mut ctx,
        OTHER,
        &createSubAccountCall { name: "n".repeat(33) }.abi_encode(),
        "name exceeds 32 bytes",
    );
    assert_eq!(create(&mut ctx, OTHER, &"n".repeat(32)), derive_sub_account(OTHER, 0));
}

/// The derived-address guard refuses state only the address's own key could have produced (an API
/// key here), but TOLERATES a gifted spot balance — otherwise anyone could block a master's next
/// sub-account forever for 1 USDC.
#[test]
fn the_derived_address_guard_refuses_keyed_state_but_tolerates_a_gift() {
    let mut ctx = make_ctx();

    let claimed = derive_sub_account(MASTER, 0);
    storage::save_api_key(
        &mut ctx,
        claimed,
        1,
        ApiKey {
            pubkey: [9u8; 32],
            expiry: 0,
        },
    )
    .unwrap();
    rejects(
        &mut ctx,
        MASTER,
        &createSubAccountCall { name: "a".into() }.abi_encode(),
        "derived sub-account address already has engine state",
    );

    // A stranger gifts 2 USDC to OTHER's NEXT derived address (it is an ordinary address until
    // created, so this is a legal external SPOT → SPOT transfer)...
    let next = derive_sub_account(OTHER, 0);
    seed(&mut ctx, MASTER, 5 * USDC, 0);
    ok(&mut ctx, MASTER, &transfer(next, WALLET_SPOT, WALLET_SPOT, 2 * USDC));
    // ...and OTHER can still create it; the gift becomes the sub's money.
    assert_eq!(create(&mut ctx, OTHER, "b"), next);
    assert_eq!(balances(&mut ctx, next), (2 * USDC, 0));
    assert_eq!(get_master(&mut ctx, next), OTHER);
}

// ── Fee rates ─────────────────────────────────────────────────────────────────

#[test]
fn fee_rates_are_copied_at_creation_and_pushed_down_by_the_admin() {
    let mut ctx = make_ctx();
    let set = |user, m, t| {
        setUserFeeRatesCall {
            user,
            makerFeeBps: m,
            takerFeeBps: t,
        }
        .abi_encode()
    };

    ok(&mut ctx, ADMIN, &set(MASTER, 2, 5));
    let sub0 = create(&mut ctx, MASTER, "a");
    assert_eq!(fee_rates(&mut ctx, sub0), (2, 5), "copied from the master at creation");
    let sub1 = create(&mut ctx, MASTER, "b");
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let out = ok(&mut ctx, ADMIN, &set(MASTER, 3, 7));
    assert_eq!(out.gas_used, 80_000);
    for u in [MASTER, sub0, sub1] {
        assert_eq!(fee_rates(&mut ctx, u), (3, 7), "{u}");
    }
    let updated: Vec<Address> = JournalTr::take_logs(ctx.journal_mut())
        .iter()
        .filter_map(|l| UserFeeRatesUpdated::decode_raw_log(l.data.topics(), &l.data.data).ok())
        .map(|e| e.user)
        .collect();
    assert_eq!(updated, vec![MASTER, sub0, sub1], "one event per account written, in order");

    rejects(&mut ctx, ADMIN, &set(sub0, 0, 0), "is a sub-account; set its master's rates");
    assert_eq!(fee_rates(&mut ctx, sub0), (3, 7));
}

// ── asSubAccount ──────────────────────────────────────────────────────────────

/// The happy path: a master places an order AS its sub. The order, its nonce and its position
/// aggregates land on the SUB; the master is untouched; the return data is placeOrder's own
/// (unwrapped); gas is the inner price plus the fixed overhead. And two subs at the same nonce get
/// different order ids.
#[test]
fn as_sub_account_places_an_order_as_the_sub() {
    let mut ctx = make_ctx();
    let sub0 = create(&mut ctx, MASTER, "a");
    let sub1 = create(&mut ctx, MASTER, "b");
    seed(&mut ctx, MASTER, 0, 10 * USDC);
    ok(&mut ctx, MASTER, &transfer(sub0, WALLET_PERP, WALLET_PERP, 3 * USDC));
    ok(&mut ctx, MASTER, &transfer(sub1, WALLET_PERP, WALLET_PERP, 3 * USDC));

    let out = ok(&mut ctx, MASTER, &as_sub(sub0, place_buy()));
    assert_eq!(out.gas_used, PLACE_ORDER_GAS + AS_SUB_ACCOUNT_OVERHEAD_GAS);
    let id0: [u8; 32] = placeOrderCall::abi_decode_returns(&out.bytes).unwrap().0;
    assert_eq!(out.bytes.len(), 32, "the inner return data verbatim, not wrapped in `bytes`");

    let order = storage::load_order(&mut ctx, &id0).unwrap().expect("rests");
    assert_eq!(Address::from(order.owner), sub0);
    assert_eq!(storage::load_position(&mut ctx, sub0, MARKET_ID).unwrap().total_buy_qty, QTY);
    assert_eq!(storage::load_position(&mut ctx, MASTER, MARKET_ID).unwrap().total_buy_qty, 0);
    assert_eq!(storage::load_account(&mut ctx, sub0).unwrap().nonce, 1);
    assert_eq!(storage::load_account(&mut ctx, MASTER).unwrap().nonce, 0);
    assert!(storage::load_user_markets(&mut ctx, MASTER).unwrap().is_empty());

    // Same nonce (0) on a sibling → a different id, and the first order is not overwritten.
    let out = ok(&mut ctx, MASTER, &as_sub(sub1, place_buy()));
    let id1: [u8; 32] = placeOrderCall::abi_decode_returns(&out.bytes).unwrap().0;
    assert_ne!(id0, id1);
    let o0 = storage::load_order(&mut ctx, &id0).unwrap().unwrap();
    let o1 = storage::load_order(&mut ctx, &id1).unwrap().unwrap();
    assert_eq!((Address::from(o0.owner), Address::from(o1.owner)), (sub0, sub1));
}

/// Only the sub's own master may wrap; every selector outside the whitelist is refused, with
/// nothing written; no recursion; a malformed inner call is refused.
#[test]
fn as_sub_account_authorization_and_whitelist() {
    let mut ctx = make_ctx();
    let sub = create(&mut ctx, MASTER, "a");
    let others_sub = create(&mut ctx, OTHER, "b");

    rejects(&mut ctx, OTHER, &as_sub(sub, place_buy()), "caller is not the master");
    rejects(&mut ctx, MASTER, &as_sub(others_sub, place_buy()), "caller is not the master");
    rejects(&mut ctx, MASTER, &as_sub(MASTER, place_buy()), "caller is not the master");
    rejects(&mut ctx, MASTER, &as_sub(OTHER, place_buy()), "caller is not the master");

    // The whitelist is exactly the 14 documented selectors, all of them tabled + dispatched.
    let table = selectors_map();
    let allowed: Vec<_> = table
        .keys()
        .copied()
        .filter(|s| sub_account_selector_allowed(*s))
        .collect();
    assert_eq!(allowed.len(), 14);

    // Every other selector in the table is refused — whatever its arguments, since the refusal
    // precedes the inner decode.
    let mut denied = 0;
    for &sel in table.keys().filter(|s| !sub_account_selector_allowed(**s)) {
        let mut inner = sel.to_vec();
        inner.extend_from_slice(&[0u8; 32 * 4]);
        let expect = if sel == depositCall::SELECTOR || sel == withdrawCall::SELECTOR {
            "cannot deposit or withdraw"
        } else {
            "inner selector is not allowed"
        };
        rejects(&mut ctx, MASTER, &as_sub(sub, inner), expect);
        denied += 1;
    }
    assert_eq!(denied, table.len() - 14);
    assert!(!sub_account_selector_allowed(asSubAccountCall::SELECTOR), "no recursion");
    assert!(!sub_account_selector_allowed(createSubAccountCall::SELECTOR), "no nesting");

    // Recursion, spelled out.
    rejects(
        &mut ctx,
        MASTER,
        &as_sub(sub, as_sub(sub, place_buy())),
        "inner selector is not allowed",
    );
    rejects(&mut ctx, MASTER, &as_sub(sub, vec![0x01, 0x02]), "inner call has no selector");
}

/// The charge is decided before dispatch: inner + overhead (a batch's dynamic cost for an inner
/// batch), the overhead floor for a refused inner call, and one gas short of the real charge is a
/// clean `OutOfGas` with no write and no log.
#[test]
fn as_sub_account_gas_is_inner_plus_overhead_and_short_gas_is_a_clean_out_of_gas() {
    let mut ctx = make_ctx();
    let sub = create(&mut ctx, MASTER, "a");
    seed(&mut ctx, MASTER, 0, 10 * USDC);
    ok(&mut ctx, MASTER, &transfer(sub, WALLET_PERP, WALLET_PERP, 5 * USDC));
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let full = PLACE_ORDER_GAS + AS_SUB_ACCOUNT_OVERHEAD_GAS;
    let writes = JournalTr::perp_write_count(ctx.journal_mut());
    let input = as_sub(sub, place_buy());
    let err = run_perp_dex_call(&input, full - 1, MASTER, U256::ZERO, false, &mut ctx).unwrap_err();
    assert_eq!(err, PerpError::OutOfGas);
    assert_eq!(JournalTr::perp_write_count(ctx.journal_mut()), writes, "zero writes");
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty(), "zero logs");
    let out = run_perp_dex_call(&input, full, MASTER, U256::ZERO, false, &mut ctx).unwrap();
    assert!(!out.reverted, "{}", revert_reason(&out));
    assert_eq!(out.gas_used, full);

    // An inner batch is billed its own dynamic cost (unknown ids are per-item rejects, not a
    // revert).
    let batch = batchCancelOrdersCall {
        orderIds: vec![FixedBytes([1u8; 32]), FixedBytes([2u8; 32])],
    }
    .abi_encode();
    let out = ok(&mut ctx, MASTER, &as_sub(sub, batch));
    assert_eq!(
        out.gas_used,
        AS_SUB_ACCOUNT_OVERHEAD_GAS + BASE_BATCH_GAS + 2 * CANCEL_ORDER_GAS
    );

    // A refused inner call pays only the floor.
    let out = call(
        &mut ctx,
        MASTER,
        &as_sub(sub, depositCall { amount: U256::from(1u64) }.abi_encode()),
    );
    assert!(out.reverted);
    assert_eq!(out.gas_used, AS_SUB_ACCOUNT_OVERHEAD_GAS);
}

/// Decision 3: API keys are PER SUB. A key registered through the wrapper lives on the sub, signs
/// for the sub, and cannot sign for the master.
#[test]
fn an_api_key_registered_for_a_sub_signs_for_the_sub_only() {
    use ed25519_dalek::{Signer, SigningKey};
    let mut ctx = make_ctx();
    let sub = create(&mut ctx, MASTER, "a");
    seed(&mut ctx, MASTER, 4 * USDC, 0);
    ok(&mut ctx, MASTER, &transfer(sub, WALLET_SPOT, WALLET_SPOT, 2 * USDC));

    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let pubkey = FixedBytes(sk.verifying_key().to_bytes());
    ok(
        &mut ctx,
        MASTER,
        &as_sub(sub, registerApiKeyCall { keyId: 1, pubkey, expiry: 0 }.abi_encode()),
    );
    let key_of = |ctx: &mut TestCtx, user| {
        let out = ok(ctx, OTHER, &getApiKeyCall { user, keyId: 1 }.abi_encode());
        getApiKeyCall::abi_decode_returns(&out.bytes).unwrap().pubkey
    };
    assert_eq!(key_of(&mut ctx, sub), pubkey);
    assert_eq!(key_of(&mut ctx, MASTER), FixedBytes::ZERO, "not the master's key");

    // "perpdex_v1_xfer_to" ‖ chainId ‖ account ‖ amount ‖ timestamp ‖ recvWindow ‖ keyId
    let signed = |account: Address, amount: u64| {
        let mut msg = b"perpdex_v1_xfer_to".to_vec();
        msg.extend_from_slice(&1u64.to_be_bytes()); // chain id of the test context
        msg.extend_from_slice(account.as_slice());
        msg.extend_from_slice(&amount.to_be_bytes());
        msg.extend_from_slice(&1u64.to_be_bytes()); // timestamp == block timestamp
        msg.extend_from_slice(&60u64.to_be_bytes());
        msg.push(1);
        transferToPerpSignedCall {
            account,
            amount,
            timestamp: 1,
            recvWindow: 60,
            keyId: 1,
            signature: sk.sign(&msg).to_bytes().to_vec().into(),
        }
        .abi_encode()
    };
    ok(&mut ctx, OTHER, &signed(sub, USDC));
    assert_eq!(balances(&mut ctx, sub), (USDC, USDC as i64));
    rejects(&mut ctx, OTHER, &signed(MASTER, USDC), "no api key registered for account");
    assert_eq!(balances(&mut ctx, MASTER), (2 * USDC, 0));
}

// ── internalTransfer ──────────────────────────────────────────────────────────

/// FAMILY: all four wallet combinations master → sub, sub → sibling and sub → master through the
/// wrapper, the one-account two-wallet move, and amounts below the external minimum — with the sum
/// of balances conserved at every step.
#[test]
fn internal_transfer_within_a_family_moves_any_wallet_combination() {
    let mut ctx = make_ctx();
    let sub0 = create(&mut ctx, MASTER, "a");
    let sub1 = create(&mut ctx, MASTER, "b");
    seed(&mut ctx, MASTER, 10 * USDC, 10 * USDC);
    let family = [MASTER, sub0, sub1];
    let sum = total(&mut ctx, &family);

    for (fw, tw) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
        ok(&mut ctx, MASTER, &transfer(sub0, fw, tw, USDC));
        assert_eq!(total(&mut ctx, &family), sum, "conserved ({fw}→{tw})");
    }
    assert_eq!(balances(&mut ctx, MASTER), (8 * USDC, 8 * 1_000_000));
    assert_eq!(balances(&mut ctx, sub0), (2 * USDC, 2 * 1_000_000));

    // sub → sibling and sub → master go through the wrapper; tiny amounts are fine in-family.
    ok(&mut ctx, MASTER, &as_sub(sub0, transfer(sub1, WALLET_PERP, WALLET_SPOT, 7)));
    ok(&mut ctx, MASTER, &as_sub(sub1, transfer(MASTER, WALLET_SPOT, WALLET_PERP, 7)));
    assert_eq!(balances(&mut ctx, sub1), (0, 0));
    // One account between its own two wallets.
    ok(&mut ctx, MASTER, &transfer(MASTER, WALLET_SPOT, WALLET_PERP, USDC));
    assert_eq!(balances(&mut ctx, MASTER), (7 * USDC, 9 * 1_000_000 + 7));
    assert_eq!(total(&mut ctx, &family), sum, "conserved");
}

/// The stream: `InternalTransfer`, then one `AccountBalanceChanged(reason = InternalTransfer)` per
/// account in ascending address order, both zero-position groups.
#[test]
fn internal_transfer_events_are_the_transfer_then_both_after_images_in_address_order() {
    let mut ctx = make_ctx();
    let sub = create(&mut ctx, MASTER, "a");
    seed(&mut ctx, MASTER, 5 * USDC, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let out = ok(&mut ctx, MASTER, &transfer(sub, WALLET_SPOT, WALLET_PERP, 2 * USDC));
    assert_eq!(out.gas_used, 50_000);
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(logs.len(), 3);
    let e = InternalTransfer::decode_raw_log(logs[0].data.topics(), &logs[0].data.data).unwrap();
    assert_eq!(
        (e.from, e.to, e.fromWallet, e.toWallet, e.amount),
        (MASTER, sub, WALLET_SPOT, WALLET_PERP, 2 * USDC)
    );
    assert_account_update_groups(&logs);
    let (lo, hi) = if MASTER < sub { (MASTER, sub) } else { (sub, MASTER) };
    assert_eq!(
        account_update_reasons(&logs),
        vec![
            (lo, AccountUpdateReason::InternalTransfer),
            (hi, AccountUpdateReason::InternalTransfer),
        ]
    );
    let after = |ctx: &mut TestCtx, user| {
        let out = ok(ctx, OTHER, &getAccountCall { user }.abi_encode());
        let a = getAccountCall::abi_decode_returns(&out.bytes).unwrap();
        (a.usdcBalance, a.totalCrossWalletBalance)
    };
    assert_eq!(after(&mut ctx, MASTER), (U256::from(3 * USDC), 0));
    assert_eq!(after(&mut ctx, sub), (U256::ZERO, 2 * USDC as i64));
}

/// EXTERNAL: SPOT → SPOT ≥ 1 USDC between two non-subs is the only shape allowed; every other
/// shape — and every malformed request — is refused with nothing written.
#[test]
fn internal_transfer_external_rules() {
    let mut ctx = make_ctx();
    let sub = create(&mut ctx, MASTER, "a");
    let others_sub = create(&mut ctx, OTHER, "b");
    seed(&mut ctx, MASTER, 10 * USDC, 10 * USDC);
    ok(&mut ctx, MASTER, &transfer(sub, WALLET_SPOT, WALLET_SPOT, 2 * USDC));

    let everyone = [MASTER, sub, OTHER, others_sub];
    let sum = total(&mut ctx, &everyone);
    ok(&mut ctx, MASTER, &transfer(OTHER, WALLET_SPOT, WALLET_SPOT, MIN_EXTERNAL_TRANSFER));
    assert_eq!(balances(&mut ctx, OTHER), (USDC, 0));
    assert_eq!(total(&mut ctx, &everyone), sum, "conserved");

    let r = |ctx: &mut TestCtx, caller, input: Vec<u8>, expect| {
        rejects(ctx, caller, &input, expect)
    };
    r(&mut ctx, MASTER, transfer(OTHER, WALLET_PERP, WALLET_SPOT, USDC), "must be SPOT to SPOT");
    r(&mut ctx, MASTER, transfer(OTHER, WALLET_SPOT, WALLET_PERP, USDC), "must be SPOT to SPOT");
    r(
        &mut ctx,
        MASTER,
        transfer(OTHER, WALLET_SPOT, WALLET_SPOT, MIN_EXTERNAL_TRANSFER - 1),
        "below the 1 USDC minimum",
    );
    r(
        &mut ctx,
        MASTER,
        as_sub(sub, transfer(OTHER, WALLET_SPOT, WALLET_SPOT, USDC)),
        "a sub-account cannot transfer outside its family",
    );
    r(
        &mut ctx,
        MASTER,
        transfer(others_sub, WALLET_SPOT, WALLET_SPOT, USDC),
        "sub-account of another family",
    );
    r(&mut ctx, MASTER, transfer(Address::ZERO, 0, 0, USDC), "zero address");
    r(&mut ctx, MASTER, transfer(PERP_DEX_ADDRESS, 0, 0, USDC), "PerpDEX itself");
    r(&mut ctx, MASTER, transfer(USDC_ADDRESS, 0, 0, USDC), "USDC token contract");
    r(&mut ctx, MASTER, transfer(MASTER, 1, 1, USDC), "same wallet");
    r(&mut ctx, MASTER, transfer(sub, 2, 0, USDC), "unknown wallet");
    r(&mut ctx, MASTER, transfer(sub, 0, 0, 0), "amount must be > 0");
    r(&mut ctx, MASTER, transfer(OTHER, 0, 0, 100 * USDC), "insufficient spot balance");
    // Deposit / withdraw through the wrapper (decision 4).
    r(
        &mut ctx,
        MASTER,
        as_sub(sub, withdrawCall { amount: U256::from(1u64) }.abi_encode()),
        "cannot deposit or withdraw",
    );
    assert_eq!(total(&mut ctx, &everyone), sum, "rejected calls moved nothing");
}

/// A PERP source is gated on DERIVED available (`perp_wallet_balance − Σ ooIM`) — the same gate as
/// `transferFromPerp` — so a resting order's requirement cannot be transferred out from under it.
#[test]
fn a_perp_source_is_gated_on_derived_available() {
    let mut ctx = make_ctx();
    let sub = create(&mut ctx, MASTER, "a");
    seed(&mut ctx, MASTER, 0, 10 * USDC);
    ok(&mut ctx, MASTER, &transfer(sub, WALLET_PERP, WALLET_PERP, 3 * USDC));
    ok(&mut ctx, MASTER, &as_sub(sub, place_buy()));

    let available = {
        let out = ok(&mut ctx, OTHER, &getAccountCall { user: sub }.abi_encode());
        getAccountCall::abi_decode_returns(&out.bytes).unwrap().availableBalance
    };
    assert!(
        available > 0 && available < 3 * USDC as i64,
        "the resting bid holds part of the wallet: {available}"
    );
    rejects(
        &mut ctx,
        MASTER,
        &as_sub(sub, transfer(MASTER, WALLET_PERP, WALLET_SPOT, available as u64 + 1)),
        "insufficient perp wallet balance",
    );
    ok(
        &mut ctx,
        MASTER,
        &as_sub(sub, transfer(MASTER, WALLET_PERP, WALLET_SPOT, available as u64)),
    );
    assert_eq!(balances(&mut ctx, MASTER), (available as u64, 7 * USDC as i64));
}

// ── Mutation-survivor killers ────────────────────────────────────────────────

/// Guards a mutation run found UNPINNED: each test's doc names the one-line change that used to
/// pass the whole suite (and now fails it).
mod mutation_killers {
    use super::*;
    use crate::types::{OrderEntry, PerpPosition};

    /// `credit_wallet`'s SPOT cap (removing it passed every test). `internalTransfer` may not push
    /// the recipient's SPOT balance past `i64::MAX` (the cap `deposit` enforces), on a family move
    /// and on an external one alike; up to the cap exactly is fine. The PERP destination has the
    /// same ceiling through `credit_perp`.
    #[test]
    fn a_credit_past_the_i64_max_ceiling_is_refused_write_free() {
        let mut ctx = make_ctx();
        let sub = create(&mut ctx, MASTER, "a");
        seed(&mut ctx, MASTER, 10 * USDC, 10 * USDC);
        seed(&mut ctx, sub, MAX_PERP_WALLET_BALANCE - 1, 0);
        seed(&mut ctx, OTHER, MAX_PERP_WALLET_BALANCE, 0);

        // Family SPOT credit: exactly up to the cap is accepted...
        ok(&mut ctx, MASTER, &transfer(sub, WALLET_SPOT, WALLET_SPOT, 1));
        assert_eq!(balances(&mut ctx, sub).0, MAX_PERP_WALLET_BALANCE);
        // ...one unit past it is refused, from either source wallet, with nothing written.
        for from_wallet in [WALLET_SPOT, WALLET_PERP] {
            rejects(
                &mut ctx,
                MASTER,
                &transfer(sub, from_wallet, WALLET_SPOT, 1),
                "destination balance would exceed i64::MAX",
            );
        }
        // External SPOT -> SPOT into an account already at the cap.
        rejects(
            &mut ctx,
            MASTER,
            &transfer(OTHER, WALLET_SPOT, WALLET_SPOT, USDC),
            "destination balance would exceed i64::MAX",
        );
        assert_eq!(balances(&mut ctx, sub).0, MAX_PERP_WALLET_BALANCE);
        assert_eq!(balances(&mut ctx, OTHER).0, MAX_PERP_WALLET_BALANCE);
        assert_eq!(balances(&mut ctx, MASTER), (10 * USDC - 1, 10 * USDC as i64));

        // PERP destination: the same ceiling, via `credit_perp`.
        seed(&mut ctx, sub, 0, i64::MAX as u64);
        rejects(
            &mut ctx,
            MASTER,
            &transfer(sub, WALLET_SPOT, WALLET_PERP, 1),
            "perp wallet: balance overflow",
        );
    }

    /// The guard's `total_position_margin != 0` clause (dropping it passed every test). Every other
    /// clause is held at its default — the derived address has no position, no market-index entry,
    /// no nonce, no key — so ONLY the margin aggregate can refuse it. (In reachable states a
    /// margined position is also in the market index, so a position-based case cannot isolate
    /// this clause.)
    #[test]
    fn the_guard_refuses_a_derived_address_carrying_position_margin_alone() {
        let mut ctx = make_ctx();
        let next = derive_sub_account(MASTER, 0);
        // The silent writer: no snapshot mark, so nothing is left pending for the call below.
        storage::mutate_account(&mut ctx, next, |a| a.total_position_margin = 1).unwrap();
        assert!(storage::load_user_markets(&mut ctx, next).unwrap().is_empty());
        assert!(storage::load_api_key_ids(&mut ctx, next).unwrap().is_empty());
        assert_eq!(storage::load_account(&mut ctx, next).unwrap().nonce, 0);

        rejects(
            &mut ctx,
            MASTER,
            &createSubAccountCall { name: "a".into() }.abi_encode(),
            "derived sub-account address already has engine state",
        );
        assert!(get_subs(&mut ctx, MASTER).is_empty());
        assert_eq!(storage::load_account(&mut ctx, next).unwrap().master, None);
    }

    /// The guard's per-user market-index clause (dropping it passed every test). Two states that
    /// put the derived address in a market's index while its ACCOUNT blob stays default — a
    /// resting-order list, and an open position whose isolated margin is exactly 0 (reachable since
    /// funding may drive isolated margin to/below zero) — so only the index clause can refuse them.
    #[test]
    fn the_guard_refuses_a_derived_address_present_in_a_market_index_alone() {
        let cases: [(&str, fn(&mut TestCtx, Address)); 2] = [
            ("resting-order list", |c, who| {
                let entries: std::collections::VecDeque<OrderEntry> = [OrderEntry {
                    order_id: [0x5a; 32],
                    price: PRICE,
                    amount: QTY,
                    maker_fee_bps: 0,
                    assuming_price: 0,
                    reduce_only: false,
                    stp: Default::default(),
                }]
                .into_iter()
                .collect();
                storage::save_buy_orders(c, who, MARKET_ID, &entries).unwrap();
            }),
            ("open position with zero isolated margin", |c, who| {
                let pos = PerpPosition {
                    amount: QTY as i64,
                    v_quote_balance: -1_000_000,
                    margin: 0,
                    leverage: 1,
                    ..PerpPosition::default()
                };
                storage::save_position(c, who, MARKET_ID, &pos, AccountUpdateReason::Adjustment)
                    .unwrap();
                // A planted fixture, not an event under test: drop its pending snapshot mark.
                storage::clear_account_snapshot_mark(c, who);
            }),
        ];
        for (what, plant) in cases {
            let mut ctx = make_ctx();
            let next = derive_sub_account(MASTER, 0);
            plant(&mut ctx, next);
            let _ = JournalTr::take_logs(ctx.journal_mut());
            assert_eq!(
                storage::load_account(&mut ctx, next).unwrap(),
                UserAccount::default(),
                "[{what}] the account blob itself must be untouched to isolate the clause"
            );
            assert_eq!(
                storage::load_user_markets(&mut ctx, next).unwrap(),
                vec![MARKET_ID],
                "[{what}]"
            );
            rejects(
                &mut ctx,
                MASTER,
                &createSubAccountCall { name: "a".into() }.abi_encode(),
                "derived sub-account address already has engine state",
            );
            assert!(get_subs(&mut ctx, MASTER).is_empty(), "[{what}]");
        }
    }

    /// The `can_be_static = false` table flag of `internalTransfer` and `createSubAccount`
    /// (flipping either passed every test, and a STATICCALL then COMMITTED its writes), and of
    /// `asSubAccount`.
    /// A STATICCALL of any of the three mutating sub-account selectors is a
    /// `StaticRestrictionViolation` with nothing written or logged; the two views stay
    /// static-callable.
    #[test]
    fn the_mutating_sub_account_selectors_are_not_static_callable() {
        let mut ctx = make_ctx();
        let sub = create(&mut ctx, MASTER, "a");
        seed(&mut ctx, MASTER, 5 * USDC, 5 * USDC);
        seed(&mut ctx, sub, 5 * USDC, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        for (name, input) in [
            ("createSubAccount", createSubAccountCall { name: "b".into() }.abi_encode()),
            ("internalTransfer", transfer(sub, WALLET_SPOT, WALLET_SPOT, USDC)),
            ("asSubAccount", as_sub(sub, transferToPerpCall { amount: 1 }.abi_encode())),
        ] {
            let writes = JournalTr::perp_write_count(ctx.journal_mut());
            let err = run_perp_dex_call(&input, 10_000_000, MASTER, U256::ZERO, true, &mut ctx)
                .unwrap_err();
            assert_eq!(err, PerpError::StaticRestrictionViolation, "[{name}]");
            assert_eq!(JournalTr::perp_write_count(ctx.journal_mut()), writes, "[{name}] wrote");
            assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty(), "[{name}] logged");
            let sel: [u8; 4] = input[..4].try_into().unwrap();
            assert!(!selectors_map()[&sel].1, "[{name}] table flag");
        }
        assert_eq!(get_subs(&mut ctx, MASTER), vec![sub], "no second sub was created");
        assert_eq!(balances(&mut ctx, MASTER), (5 * USDC, 5 * USDC as i64));
        assert_eq!(balances(&mut ctx, sub), (5 * USDC, 0));

        for input in [
            getSubAccountsCall { master: MASTER }.abi_encode(),
            getMasterCall { account: sub }.abi_encode(),
        ] {
            let out = run_perp_dex_call(&input, 10_000_000, OTHER, U256::ZERO, true, &mut ctx)
                .expect("a view is static-callable");
            assert!(!out.reverted);
        }
    }
}

/// Self-trade prevention is keyed on the ADDRESS, not the family (plan T20). A sub-account is a
/// different address from its master, so a master's resting order is FILLED by the sub's GTC
/// (and vice versa) under the default `EXPIRE_MAKER` — never cancelled — on both sides.
#[test]
fn a_master_and_its_sub_trade_with_each_other_under_stp() {
    use crate::interface::IPerpDex::{OrderCancelled, Trade};
    let order = |side: u8| {
        placeOrderCall {
            marketId: MARKET_ID,
            side,
            price: PRICE,
            quantity: QTY,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
            flags: 0,
        }
        .abi_encode()
    };
    for maker_side in [0u8, 1] {
        for master_rests in [true, false] {
            let mut ctx = make_ctx();
            let sub = create(&mut ctx, MASTER, "desk");
            seed(&mut ctx, MASTER, 0, 10 * USDC);
            ok(&mut ctx, MASTER, &transfer(sub, WALLET_PERP, WALLET_PERP, 5 * USDC));
            let (maker_in, taker_in) = (order(maker_side), order(1 - maker_side));
            let (maker, taker) = if master_rests {
                ok(&mut ctx, MASTER, &maker_in);
                (MASTER, sub)
            } else {
                ok(&mut ctx, MASTER, &as_sub(sub, maker_in));
                (sub, MASTER)
            };
            let _ = JournalTr::take_logs(ctx.journal_mut());
            if master_rests {
                ok(&mut ctx, MASTER, &as_sub(sub, taker_in));
            } else {
                ok(&mut ctx, MASTER, &taker_in);
            }
            let logs = JournalTr::take_logs(ctx.journal_mut());
            let trades: Vec<(Address, Address)> = logs
                .iter()
                .filter(|l| l.data.topics().first() == Some(&Trade::SIGNATURE_HASH))
                .map(|l| {
                    let t = Trade::decode_raw_log(l.data.topics(), &l.data.data).unwrap();
                    (t.maker, t.taker)
                })
                .collect();
            let case = format!("maker side {maker_side}, master rests {master_rests}");
            assert_eq!(trades, vec![(maker, taker)], "{case}: a fill, not a prevention");
            assert!(
                !logs
                    .iter()
                    .any(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH)),
                "{case}: nothing cancelled"
            );
        }
    }
}
