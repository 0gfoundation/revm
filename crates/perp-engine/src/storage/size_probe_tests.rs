use super::*;
use crate::types::{
    MarginTiers, OrderStatus, OrderType, Side, TimeInForce, PRICE_BASIS_WINDOW_SIZE,
};

#[test]
fn probe_encoded_sizes() {
    // Order — realistic BTC-ish values
    let order = Order {
        owner: [0xAB; 20],
        market_id: 1,
        side: Side::Buy,
        price: 65_432_10,      // 7 digits
        quantity: 150_000_000, // 1.5 BTC @ 8 decimals
        filled: 50_000_000,
        order_type: OrderType::Limit,
        tif: TimeInForce::Gtc,
        status: OrderStatus::PartiallyFilled,
                reduce_only: false,
                trigger_price: 0,
                trigger_up: false,
                close_position: false,
                stp: Default::default(),
                position_generation: 0,
                trail_bps: 0,
    };
    let buf = encode(&order).unwrap();
    println!(
        "Order: {} bytes; hex={}",
        buf.len(),
        primitives::hex::encode(&buf)
    );

    let entry = OrderEntry {
        order_id: [0xCD; 32],
        price: 65_432_10,
        amount: 150_000_000,
        maker_fee_ppm: 200,
        assuming_price: 6_550_000,
                reduce_only: false,
                stp: Default::default(),
    };
    println!(
        "OrderEntry x1 (in vec): {} bytes",
        encode(&vec![entry]).unwrap().len()
    );
    println!(
        "OrderEntry x5: {} bytes",
        encode(&vec![entry; 5]).unwrap().len()
    );
    println!(
        "OrderEntry x20: {} bytes",
        encode(&vec![entry; 20]).unwrap().len()
    );
    println!(
        "OrderEntry single hex={}",
        primitives::hex::encode(encode(&entry).unwrap())
    );

    let prices: Vec<u64> = (0..1u64).map(|i| 65_000_00 + i * 10).collect();
    println!("bid_prices x1: {} bytes", encode(&prices).unwrap().len());
    let prices: Vec<u64> = (0..10u64).map(|i| 65_000_00 + i * 10).collect();
    println!("bid_prices x10: {} bytes", encode(&prices).unwrap().len());
    let prices: Vec<u64> = (0..100u64).map(|i| 65_000_00 + i * 10).collect();
    println!("bid_prices x100: {} bytes", encode(&prices).unwrap().len());

    let q: Vec<[u8; 32]> = vec![[0xEF; 32]; 1];
    println!("level queue x1: {} bytes", encode(&q).unwrap().len());
    let q: Vec<[u8; 32]> = vec![[0xEF; 32]; 5];
    println!("level queue x5: {} bytes", encode(&q).unwrap().len());
    println!(
        "level queue single elem hex={}",
        primitives::hex::encode(encode(&[0xEFu8; 32]).unwrap())
    );

    let pos = PerpPosition {
        amount: 150_000_000,
        v_quote_balance: -98_148_315,
        margin: 9_814_831,
        leverage: 10,
        last_funding_index: 123_456_789_012_345i128,
        total_buy_qty: 60_000_000,
        total_buy_notional: 50_000_000,
        total_sell_qty: 12_000_000,
        total_sell_notional: 10_000_000,
        cumulative_realized_pnl: -4_200_000,
        generation: 3,
        margin_type: 1,
    };
    let buf = encode(&pos).unwrap();
    println!(
        "PerpPosition: {} bytes; hex={}",
        buf.len(),
        primitives::hex::encode(&buf)
    );
    println!(
        "PerpPosition default: {} bytes",
        encode(&PerpPosition::default()).unwrap().len()
    );

    let acct = UserAccount {
        usdc_balance: "123456789000000000000".into(), // 21-digit decimal string
        perp_wallet_balance: 1_234_567_890,
        maker_fee_ppm: 200,
        taker_fee_ppm: 500,
        nonce: 7,
        total_position_margin: 4_200_000,
        master: None,
        sub_count: 0,
        liq_flags: 0,
        open_position_count: 0,
    };
    let buf = encode(&acct).unwrap();
    println!(
        "UserAccount: {} bytes; hex={}",
        buf.len(),
        primitives::hex::encode(&buf)
    );

    let market = Market {
        market_id: 1,
        base_decimals: 8,
        price_decimals: 2,
        tick_size: 10,
        step_size: 1000,
        min_quantity: 1000,
        max_quantity: 10_000_000_000,
        max_price: 100_000_000,
        price_update_interval: 1,
        active: true,
        funding_interval: 28_800,
        interest_rate: 100,
        liquidation_fee_rate_bps: 50,
        price_band_bps: 0,
        mark_price: 0,
        tiers: MarginTiers::default(),
        basis_mode: 0,
        deferred_work: 0,
        cumulative_funding_index: 0,
    };
    let buf = encode(&market).unwrap();
    println!("Market: {} bytes", buf.len());

    let fs = FundingState {
        last_funding_rate: 125,
        next_funding_ts: 1_750_000_000,
        cumulative_funding_index: 9_876_543_210_123i128,
    };
    println!("FundingState: {} bytes", encode(&fs).unwrap().len());

    let acc = PremiumIndexAccumulator {
        weighted_sum: 123_456_789_012i128,
        sample_count: 28_000,
        epoch_start_ts: 1_750_000_000,
        last_pi: -1234,
        last_sample_ts: 1_750_000_123,
    };
    println!(
        "PremiumIndexAccumulator: {} bytes",
        encode(&acc).unwrap().len()
    );

    let mut window = PriceBasisWindow::default();
    for i in 0..PRICE_BASIS_WINDOW_SIZE as u64 {
        let _ = window.record_observation(1_750_000_000 + i, 65_000_00 + i);
    }
    println!(
        "PriceBasisWindow full: {} bytes",
        encode(&window).unwrap().len()
    );
    println!(
        "PriceBasisWindow empty: {} bytes",
        encode(&PriceBasisWindow::default()).unwrap().len()
    );

    let mut hist = IndexPriceHistory::default();
    for i in 0..32u64 {
        hist.push(
            IndexPriceState {
                index_price: 65_000_00 + i,
                timestamp: 1_750_000_000 + i,
            },
            32,
        );
    }
    println!(
        "IndexPriceHistory x32: {} bytes",
        encode(&hist).unwrap().len()
    );

    println!(
        "UserFeeRates: {} bytes",
        encode(&UserFeeRates {
            maker_fee_ppm: 200,
            taker_fee_ppm: 500
        })
        .unwrap()
        .len()
    );
    println!(
        "u64 scalar (mark price 6_543_210): {} bytes",
        encode(&6_543_210u64).unwrap().len()
    );
    println!(
        "u64 scalar small (nonce 7): {} bytes",
        encode(&7u64).unwrap().len()
    );
    println!("Address: {} bytes", encode(&Address::ZERO).unwrap().len());
    println!(
        "ApiKey: {} bytes",
        encode(&ApiKey {
            pubkey: [9; 32],
            expiry: 1_750_000_000
        })
        .unwrap()
        .len()
    );
}
