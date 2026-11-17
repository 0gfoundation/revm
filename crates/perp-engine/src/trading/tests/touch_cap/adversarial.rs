//! Adversarial cases for the Q1 touch cap (from the breaker pass). Each one tries a way to make a
//! single call touch more than 64 live resting orders, or to make a capped / rejected call leak
//! state. The last one pins a known gap the cap does not cover (reduce-only evictions).
use super::*;
use crate::interface::IPerpDex::{asSubAccountCall, createSubAccountCall};

const RO: u8 = 1;

fn mk(i: usize) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0x7e;
    b[12..].copy_from_slice(&(i as u64).to_be_bytes());
    Address::from(b)
}

/// Live touches a call made: every maker `Trade` plus every `OrderCancelled` of an order that is
/// not the taker's own new order.
fn live_touches(logs: &[primitives::Log]) -> usize {
    fills(logs).len() + cancels(logs).len()
}

/// Interleaved stale / own-STP / frozen / K9 / live makers on one level and on a ladder. Whatever
/// the mix, the IOC touches exactly 64 live makers and a GTC is refused write-clean.
#[test]
fn brk_mixed_stale_stp_frozen_k9_live_never_exceed_64() {
    let eve = mk(1);
    for d in DIRS {
        for l in LAYOUTS {
            let tag = format!("{d:?} {l:?}");
            let mut ctx = fresh();
            set_wallet(&mut ctx, eve, WALLET);
            // 5 kinds × 30 = 150 ids: CAROL (to be cancelled → stale), ALICE (own, EM), DAVE
            // (frozen), EVE (K9: wallet 0), BOB (live).
            let mut owners = Vec::new();
            for _ in 0..30 {
                owners.extend([CAROL, ALICE, DAVE, eve, BOB]);
            }
            let ids = seed(&mut ctx, d, l, &owners);
            for (k, id) in ids.iter().enumerate() {
                if owners[k] == CAROL {
                    ok(&mut ctx, CAROL, &cancelOrderCall { orderId: (*id).into(), marketId: MARKET_ID }
                        .abi_encode());
                }
            }
            freeze(&mut ctx, DAVE);
            set_wallet(&mut ctx, eve, 0);

            rejected(&mut ctx, ALICE, &limit(d.taker(), d.through(), 1_000, GTC, EM), TOUCH_CAP_REJECT);
            rejected(&mut ctx, BOB, &limit(d.taker(), d.through(), 1_000, FOK, 0), TOUCH_CAP_REJECT);

            let (out, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.through(), 1_000, IOC, EM));
            assert_eq!(live_touches(&logs), CAP, "{tag}");
            let (_, f, e, r) = expiry(&logs);
            assert_eq!((f + e, r), (1_000, X_TIF), "{tag}");
            assert!(storage::load_order(&mut ctx, &order_id_of(&out)).unwrap().is_none());
            // 4 live per group of 5: 64 touches = 16 groups, the trip maker is the first live one
            // of group 17 (ALICE's own order, which STP did NOT see).
            let trip = ids[16 * 5 + 1];
            assert!(storage::load_order(&mut ctx, &trip).unwrap().is_some(), "{tag}: trip maker kept");
        }
    }
}

/// Reduce-only truncation does not widen the walk: ALICE long 100 sends a reduce-only sell of 1000.
/// It is truncated to 100; an IOC fills 64 and expires 36; a GTC is refused write-clean.
#[test]
fn brk_reduce_only_truncation_then_cap() {
    let mut ctx = fresh();
    rest(&mut ctx, CAROL, SELL, MARK, 100);
    ok(&mut ctx, ALICE, &limit(BUY, MARK, 100, IOC, 0));
    assert_eq!(amount(&mut ctx, ALICE), 100);
    let mut owners = vec![BOB; 150];
    owners.extend([DAVE; 50]);
    seed(&mut ctx, Dir(SELL), Layout::Ladder, &owners);
    rejected(&mut ctx, ALICE, &limit(SELL, MARK - 500, 1_000, GTC, RO), TOUCH_CAP_REJECT);
    let (out, logs) = ok(&mut ctx, ALICE, &limit(SELL, MARK - 500, 1_000, IOC, RO));
    assert_eq!(fills(&logs).len(), CAP);
    assert_eq!(expiry(&logs), (order_id_of(&out), 64, 36, X_TIF));
    assert_eq!(amount(&mut ctx, ALICE), 36);
    // Reduce-only MARKET: truncated to the 36 left, no cap involvement.
    let (_, logs) = ok(&mut ctx, ALICE, &place(SELL, 0, 1_000, MKT, GTC, RO));
    assert_eq!(fills(&logs).len(), 36);
    assert_eq!(amount(&mut ctx, ALICE), 0);
    // A plain market sell of 1000 then opens a short — capped at 64.
    let (out, logs) = ok(&mut ctx, ALICE, &market(SELL, 1_000));
    assert_eq!(fills(&logs).len(), CAP);
    assert_eq!(expiry(&logs), (order_id_of(&out), 64, 936, X_TIF));
}

/// Off-band makers are never loaded, so they cannot be used to inflate or deflate the count.
#[test]
fn brk_band_edge_off_band_makers_not_counted() {
    let mut ctx = fresh();
    // 100 asks placed while the mark was 8_000, then the mark moves to 10_000: they now sit below
    // mark - band (9_000) and a buy walk skips them.
    storage::save_mark_price(&mut ctx, MARKET_ID, 8_000).unwrap();
    for _ in 0..100 {
        rest(&mut ctx, CAROL, SELL, 8_000, 1);
    }
    storage::save_mark_price(&mut ctx, MARKET_ID, MARK).unwrap();
    let ids = seed(&mut ctx, Dir(BUY), Layout::Ladder, &[BOB; 65]);
    let (_, logs) = ok(&mut ctx, ALICE, &limit(BUY, 10_999, 64, GTC, 0));
    assert_eq!(fills(&logs), one_each(&ids[..64]));
    let (out, logs) = ok(&mut ctx, ALICE, &limit(BUY, 11_000, 1_000, IOC, 0));
    assert_eq!(fills(&logs), one_each(&ids[64..]));
    assert_eq!(expiry(&logs), (order_id_of(&out), 1, 999, X_TIF));
    assert_eq!(level(&mut ctx, SELL, 8_000).1, 100);
}

/// A batch whose items each start with STP cancels: cancels are touches and draw on the 256 total.
#[test]
fn brk_batch_stp_cancels_draw_on_the_batch_total() {
    for d in DIRS {
        let mut ctx = fresh();
        let mut owners = vec![ALICE; 150];
        owners.extend([BOB; 160]);
        owners.extend([CAROL; 160]);
        seed(&mut ctx, d, Layout::OneLevel, &owners);
        let mut it = ioc(d, 64);
        it.flags = EM;
        let (out, logs) = ok(&mut ctx, ALICE, &batch(vec![it; 6]));
        assert_eq!(live_touches(&logs), 256, "{d:?}");
        assert_eq!(cancels(&logs).len(), 150, "{d:?}");
        assert_eq!(fills(&logs).len(), 106, "{d:?}");
        assert_eq!(tags(&out), vec![(TAG_TERMINAL, R_NONE); 6], "{d:?}");
    }
}

/// Interleaved accepted / rejected items. Every item spends what it walked: IOC 64 (left 192), GTC
/// 65 refused after 64 (left 128), FOK 65 refused by its pre-check after 64 (left 64), IOC 64 (left
/// 0), then a GTC refused at its first maker and three IOC items that fill nothing.
#[test]
fn brk_batch_interleaved_rejects_and_fills() {
    for d in DIRS {
        let mut ctx = fresh();
        let ids = deep_book(&mut ctx, d);
        let fok = item(d.taker(), d.through(), 65, LIMIT, FOK);
        let items = vec![
            ioc(d, 64),
            gtc(d, 65),
            fok,
            ioc(d, 64),
            gtc(d, 65),
            ioc(d, 64),
            ioc(d, 64),
            ioc(d, 64),
        ];
        let (out, logs) = ok(&mut ctx, ALICE, &batch(items));
        assert_eq!(
            tags(&out),
            vec![
                (TAG_TERMINAL, R_NONE),
                (TAG_REJECTED, R_OTHER),
                (TAG_REJECTED, R_OTHER),
                (TAG_TERMINAL, R_NONE),
                (TAG_REJECTED, R_OTHER),
                (TAG_TERMINAL, R_NONE),
                (TAG_TERMINAL, R_NONE),
                (TAG_TERMINAL, R_NONE),
            ],
            "{d:?}"
        );
        assert_eq!(fills(&logs), one_each(&ids[..128]), "{d:?}");
        let recs = records(&out);
        let e: Vec<_> = evs::<OrderExpired>(&logs)
            .iter()
            .map(|e| (e.orderId.0, e.filledQuantity, e.expiredQuantity, e.reason))
            .collect();
        let want: Vec<_> = (5..8).map(|k| (recs[k].1, 0, 64, X_TIF)).collect();
        assert_eq!(e, want, "{d:?}");
    }
}

/// `asSubAccount(batchPlaceOrders)` keeps the per-batch total.
#[test]
fn brk_as_sub_account_batch_keeps_the_total() {
    for d in DIRS {
        let mut ctx = fresh();
        let (out, _) = ok(&mut ctx, ALICE, &createSubAccountCall { name: "desk".into() }.abi_encode());
        let sub = createSubAccountCall::abi_decode_returns(&out.bytes).unwrap();
        set_wallet(&mut ctx, sub, WALLET);
        let ids = deep_book(&mut ctx, d);
        let wrapped =
            asSubAccountCall { subAccount: sub, data: batch(vec![ioc(d, 64); 5]).into() }.abi_encode();
        let (_, logs) = ok(&mut ctx, ALICE, &wrapped);
        assert_eq!(fills(&logs), one_each(&ids[..256]), "{d:?}");
        assert_eq!(storage::load_position(&mut ctx, sub, MARKET_ID).unwrap().amount, 256 * d.sign());
        // And a single wrapped GTC over the cap is refused write-clean (64 left + 1 more).
        seed(&mut ctx, d, Layout::OneLevel, &[DAVE]);
        let wrapped =
            asSubAccountCall { subAccount: sub, data: limit(d.taker(), d.through(), 65, GTC, 0).into() }
                .abi_encode();
        rejected(&mut ctx, ALICE, &wrapped, TOUCH_CAP_REJECT);
    }
}

/// Signed IOC over the cap: same event shape as the direct one.
#[test]
fn brk_signed_ioc_clamps_with_the_direct_event_shape() {
    for d in DIRS {
        let mut ctx = fresh();
        register_alice_key(&mut ctx);
        let ids = seed(&mut ctx, d, Layout::Ladder, &[BOB; 65]);
        let input = signed_place(&mut ctx, d.taker(), d.through(), 100, IOC, 0);
        let (_, logs) = ok(&mut ctx, DAVE, &input);
        assert_eq!(fills(&logs), one_each(&ids[..64]), "{d:?}");
        let (_, f, e, r) = expiry(&logs);
        assert_eq!((f, e, r), (64, 36, X_TIF), "{d:?}");
    }
}

/// Cancel-replace churn at one level: 60 cancelled-then-replaced makers leave 60 stale ids among
/// 65 live ones. The stale ids are free; the live count alone decides.
#[test]
fn brk_cancel_replace_churn_at_one_level() {
    for d in DIRS {
        let mut ctx = fresh();
        let ids = seed(&mut ctx, d, Layout::OneLevel, &[BOB; 65]);
        let mut live: Vec<[u8; 32]> = ids[60..].to_vec();
        for id in &ids[..60] {
            ok(&mut ctx, BOB, &cancelOrderCall { orderId: (*id).into(), marketId: MARKET_ID }.abi_encode());
            live.push(rest(&mut ctx, BOB, d.maker(), d.lvl(0), 1));
        }
        rejected(&mut ctx, ALICE, &limit(d.taker(), d.through(), 65, GTC, 0), TOUCH_CAP_REJECT);
        let (_, logs) = ok(&mut ctx, ALICE, &limit(d.taker(), d.through(), 64, GTC, 0));
        assert_eq!(fills(&logs), one_each(&live[..64]), "{d:?}");
    }
}

/// KNOWN GAP, pinned (predates Q1; open follow-up): the post-flush reduce-only eviction pass is not
/// bounded by the touch cap. Each maker's ONE in-band fill can flatten it and evict up to 199 of its
/// resting reduce-only orders that sit off-band (never walked). One 3-touch IOC here cancels
/// 3 × 199 = 597 orders; at 64 touches the same shape cancels 64 × 199 = 12,736 in one flat-gas
/// call. When this is bounded, this test changes with it.
#[test]
fn brk_known_gap_reduce_only_evictions_are_not_bounded_by_the_cap() {
    let mut ctx = fresh();
    const N: usize = 3;
    // Liquidity for the makers to get long 199 each.
    rest(&mut ctx, CAROL, SELL, MARK, 199 * N as u64);
    for i in 0..N {
        let m = mk(100 + i);
        set_wallet(&mut ctx, m, WALLET);
        ok(&mut ctx, m, &limit(BUY, MARK, 199, IOC, 0));
        assert_eq!(amount(&mut ctx, m), 199);
    }
    // 199 reduce-only asks per maker at 8_000, placed while the mark was there (a later mark move
    // of >= the 10% band leaves them below the buy walk's lower band, 9_000, so walks skip them).
    storage::save_mark_price(&mut ctx, MARKET_ID, 8_000).unwrap();
    for i in 0..N {
        for _ in 0..199 {
            let (_, logs) = ok(&mut ctx, mk(100 + i), &limit(SELL, 8_000, 1, GTC, RO));
            assert_eq!(evs::<OrderRested>(&logs).len(), 1);
        }
    }
    // Each maker's one NORMAL ask of 199, in band (one level each, farther than the RO asks, so it
    // does not overtake them and evicts nothing at placement).
    let normals: Vec<_> = (0..N).map(|i| rest(&mut ctx, mk(100 + i), SELL, MARK + 1 + i as u64, 199)).collect();
    storage::save_mark_price(&mut ctx, MARKET_ID, MARK).unwrap();
    assert_eq!(level(&mut ctx, SELL, 8_000).1, 199 * N as u64);
    let (_, logs) = ok(&mut ctx, ALICE, &limit(BUY, MARK + 10, 199 * N as u64, IOC, 0));
    assert_eq!(fills(&logs), normals.iter().map(|&id| (id, 199)).collect::<Vec<_>>());
    let c = cancels(&logs);
    assert!(c.len() > CAP, "{} evictions from a {N}-touch call", c.len());
    assert_eq!(c.len(), 199 * N);
}
