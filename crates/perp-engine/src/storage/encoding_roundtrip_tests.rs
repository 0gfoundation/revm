use super::*;
use crate::types::{MarginTiers, OrderStatus, OrderType, Side, TimeInForce};

fn rt<T>(label: &str, v: T)
where
    T: serde::Serialize + for<'de> Deserialize<'de> + PartialEq + core::fmt::Debug,
{
    let buf = encode(&v).unwrap();
    let back: T = decode(&buf).unwrap();
    assert_eq!(v, back, "round-trip mismatch for {label}");
}

#[test]
fn order_every_enum_combo_and_high_byte_owner() {
    for side in [Side::Buy, Side::Sell] {
        for ot in [OrderType::Limit, OrderType::Market] {
            for tif in [
                TimeInForce::Gtc,
                TimeInForce::Ioc,
                TimeInForce::Fok,
                TimeInForce::PostOnly,
            ] {
                for status in [
                    OrderStatus::Open,
                    OrderStatus::PartiallyFilled,
                    OrderStatus::Filled,
                    OrderStatus::Cancelled,
                    OrderStatus::Expired,
                ] {
                    // owner spans the full byte domain incl. > 0x7F.
                    let mut owner = [0u8; 20];
                    for (i, b) in owner.iter_mut().enumerate() {
                        *b = (0x80 + i as u8) ^ 0xA5;
                    }
                    rt(
                        "Order",
                        Order {
                            owner,
                            market_id: u64::MAX,
                            side,
                            price: u64::MAX,
                            quantity: u64::MAX - 1,
                            filled: 1,
                            order_type: ot,
                            tif,
                            status,
                                                        reduce_only: false,
                                                        trigger_price: 0,
                                                        trigger_up: false,
                                                        close_position: false,
                                                        stp: Default::default(),
                                                        position_generation: u64::MAX,
                                                        trail_bps: u16::MAX,
                        },
                    );
                }
            }
        }
    }
}

#[test]
fn order_entry_and_api_key_high_byte_arrays() {
    let id: [u8; 32] = core::array::from_fn(|i| (0xFF - i as u8) ^ 0x3C);
    rt(
        "OrderEntry",
        OrderEntry {
            order_id: id,
            price: u64::MAX,
            amount: u64::MAX,
            maker_fee_ppm: -12_345,
            assuming_price: u64::MAX,
                        reduce_only: false,
                        stp: Default::default(),
        },
    );
    rt(
        "ApiKey",
        ApiKey {
            pubkey: id,
            expiry: u64::MAX,
        },
    );
    rt(
        "ApiKey-never-expires",
        ApiKey {
            pubkey: [0xFF; 32],
            expiry: 0,
        },
    );
}

#[test]
fn perp_position_signed_extremes() {
    rt(
        "PerpPosition",
        PerpPosition {
            amount: i64::MIN,
            v_quote_balance: i64::MAX,
            margin: -1,
            leverage: 20,
            last_funding_index: i128::MIN,
            total_buy_qty: u64::MAX,
            total_buy_notional: u64::MAX,
            total_sell_qty: 0,
            total_sell_notional: u64::MAX,
            // The signed extreme on the accumulator too: it is the one field that grows
            // monotonically over a position's whole life, so its round-trip at `i64::MIN`
            // matters more than most.
            cumulative_realized_pnl: i64::MIN,
            generation: u64::MAX,
            // ISOLATED: the non-default value, so the round-trip proves it is not dropped.
            margin_type: 1,
        },
    );
    rt(
        "PerpPosition-pos-i128",
        PerpPosition {
            last_funding_index: i128::MAX,
            ..PerpPosition::default()
        },
    );
}

#[test]
fn market_all_fields_incl_defaulted() {
    rt(
        "Market",
        Market {
            market_id: u64::MAX,
            base_decimals: u32::MAX,
            price_decimals: 18,
            tick_size: u64::MAX,
            step_size: 1,
            min_quantity: 1,
            max_quantity: u64::MAX,
            max_price: u64::MAX,
            price_update_interval: 15,
            active: true,
            funding_interval: 3600,
            interest_rate: i64::MIN,
            liquidation_fee_rate_bps: u32::MAX,
            price_band_bps: 0,
            mark_price: u64::MAX,
            tiers: MarginTiers::default(),
            basis_mode: 0,
            deferred_work: 0,
            cumulative_funding_index: 0,
        },
    );
}

#[test]
fn funding_and_premium_signed_i128() {
    rt(
        "FundingState",
        FundingState {
            last_funding_rate: i64::MIN,
            next_funding_ts: u64::MAX,
            cumulative_funding_index: i128::MIN,
        },
    );
    rt(
        "PremiumIndexAccumulator",
        PremiumIndexAccumulator {
            weighted_sum: i128::MIN,
            sample_count: u64::MAX,
            epoch_start_ts: 1,
            last_pi: i64::MIN,
            last_sample_ts: u64::MAX,
        },
    );
}

#[test]
fn enum_out_of_range_discriminant_errors_cleanly() {
    // Each enum round-trips its real discriminants...
    for s in [OrderStatus::Open, OrderStatus::Filled, OrderStatus::Expired] {
        let buf = encode(&s).unwrap();
        assert_eq!(decode::<OrderStatus>(&buf).unwrap(), s);
    }
    // ...and an out-of-range discriminant decodes to a clean Err, never UB/panic.
    // 99 encodes as a 1-byte msgpack positive fixint (0x63); no OrderStatus variant == 99.
    assert!(
        decode::<OrderStatus>(&[0x63]).is_err(),
        "out-of-range enum discriminant must error, not panic/UB"
    );
    assert!(decode::<Side>(&[0x05]).is_err());
}

#[test]
fn order_id_queue_pack_unpack_roundtrip_and_rejects_misaligned() {
    for q in [
        vec![],
        vec![[0xFFu8; 32]],
        vec![
            [0x00u8; 32],
            [0x80u8; 32],
            core::array::from_fn(|i| i as u8),
        ],
    ] {
        let packed = pack_order_ids(&q);
        assert_eq!(packed.len(), q.len() * 32);
        assert_eq!(
            unpack_order_ids(&packed).unwrap(),
            q,
            "pack/unpack must round-trip"
        );
    }
    // A blob whose length is not a multiple of 32 is rejected, never silently truncated.
    for bad_len in [1usize, 31, 33, 63] {
        assert!(
            unpack_order_ids(&vec![0xABu8; bad_len]).is_err(),
            "len {bad_len} must error"
        );
    }
}
