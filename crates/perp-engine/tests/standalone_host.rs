//! End-to-end smoke test of the engine on [`InMemoryHost`] — no revm context, no journal.
//! Doubles as the usage example for standalone consumers (perf benches, other services).

use alloy_sol_types::SolCall;
use perp_core::compute_block_commitment;
use perp_engine::{
    interface::IPerpDex::{cancelOrderCall, placeOrderCall},
    run_perp_dex_call, storage,
    types::{AccountUpdateReason, MarginTiers, Market},
    InMemoryHost, PerpHost, USDC_ADDRESS,
};
use primitives::{address, Address, FixedBytes, U256};

const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const ALICE: Address = address!("2000000000000000000000000000000000000001");
const BOB: Address = address!("2000000000000000000000000000000000000002");
const MARKET_ID: u64 = 1;

fn market() -> Market {
    Market {
        market_id: MARKET_ID,
        base_decimals: 5,
        price_decimals: 2,
        tick_size: 10,
        step_size: 1,
        min_quantity: 1,
        max_quantity: 100_000_000,
        max_price: 1_000_000_000,
        price_update_interval: 15,
        active: true,
        funding_interval: 0,
        interest_rate: 0,
        liquidation_fee_rate_bps: 0,
        price_band_bps: 1_000_000, // disabled
        mark_price: 0,
        tiers: MarginTiers::default(),
    }
}

fn fund(host: &mut InMemoryHost, user: Address, amount: u64) {
    let mut acc = storage::load_account(host, user).unwrap();
    acc.credit_perp(amount).unwrap();
    storage::save_account(host, user, acc, AccountUpdateReason::Adjustment).unwrap();
}

fn place(host: &mut InMemoryHost, user: Address, side: u8, price: u64, qty: u64) -> [u8; 32] {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: 0, // limit
        tif: 0,       // GTC
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let out = run_perp_dex_call(&input, u64::MAX, user, U256::ZERO, false, host).unwrap();
    assert!(!out.reverted, "place reverted: {:?}", out.bytes);
    out.bytes[..32].try_into().unwrap()
}

#[test]
fn place_match_cancel_and_block_commitment() {
    let mut host = InMemoryHost::new(1_700_000_000);
    storage::save_admin(&mut host, ADMIN).unwrap();
    storage::save_market(&mut host, &market()).unwrap();
    fund(&mut host, ALICE, 1_000_000_000_000_000_000);
    fund(&mut host, BOB, 1_000_000_000_000_000_000);
    let _ = USDC_ADDRESS; // deposit/withdraw bridge unused here (funded directly)

    // Bob rests an ask 2 @ $500.00; Alice's buy 3 crosses: fills 2, rests 1.
    let _bob_ask = place(&mut host, BOB, 1, 50_000, 2);
    let alice_bid = place(&mut host, ALICE, 0, 50_000, 3);

    let alice = storage::load_order_ref(&mut host, &alice_bid).unwrap().expect("resting");
    assert_eq!(alice.filled, 2);
    assert_eq!(alice.quantity, 3);

    // Events flowed through the host sink (OrderPlaced ×2 + Trade + position/balance events).
    assert!(host.logs.iter().count() >= 4, "expected events, got {}", host.logs.len());

    // Cancel the remainder through the full shell.
    let cancel = cancelOrderCall { orderId: FixedBytes(alice_bid), marketId: MARKET_ID }.abi_encode();
    let out = run_perp_dex_call(&cancel, u64::MAX, ALICE, U256::ZERO, false, &mut host).unwrap();
    assert!(!out.reverted);

    // Block end: harvest the net delta, fold the chained commitment, state survives
    // into the committed store (cold reads keep working next block).
    let delta = host.end_block();
    assert!(!delta.is_empty());
    let c1 = compute_block_commitment(U256::ZERO, &delta);
    assert_ne!(c1, U256::ZERO);

    // Next block: cold read through the committed store still sees the market.
    let m = storage::load_market(&mut host, MARKET_ID).unwrap().expect("market survives");
    assert!(m.active);

    // Determinism: an identical run produces the identical commitment.
    let mut h2 = InMemoryHost::new(1_700_000_000);
    storage::save_admin(&mut h2, ADMIN).unwrap();
    storage::save_market(&mut h2, &market()).unwrap();
    fund(&mut h2, ALICE, 1_000_000_000_000_000_000);
    fund(&mut h2, BOB, 1_000_000_000_000_000_000);
    let _ = place(&mut h2, BOB, 1, 50_000, 2);
    let bid2 = place(&mut h2, ALICE, 0, 50_000, 3);
    let cancel2 = cancelOrderCall { orderId: FixedBytes(bid2), marketId: MARKET_ID }.abi_encode();
    run_perp_dex_call(&cancel2, u64::MAX, ALICE, U256::ZERO, false, &mut h2).unwrap();
    let c2 = compute_block_commitment(U256::ZERO, &h2.end_block());
    assert_eq!(c1, c2, "same op stream → same chained commitment");
}
