use super::*;
use crate::{math::calc_value, run_perp_dex_call};

const N_ACCT: u64 = 12;
const ACCT_WALLET: u64 = 3_000_000; // TIGHT: forces wallet-cover cancels + insolvency rejects
/// Coverage floor on self-trade-prevention cancels — about half of the 37 the seeded pass
/// reaches.
const STP_FLOOR_CONSERVATION: u32 = 18;

fn addr(i: u64) -> Address {
    let mut b = [0u8; 20];
    b[11] = 0xE0;
    b[12..20].copy_from_slice(&i.to_be_bytes());
    Address::from(b)
}

fn total_equity(ctx: &mut TestCtx) -> i128 {
    let mut s: i128 = 0;
    for i in 0..N_ACCT {
        let a = addr(i);
        let acc = storage::load_account(ctx, a).unwrap();
        s += acc.perp_wallet_balance as i128;
        let p = storage::load_position(ctx, a, MARKET_ID).unwrap();
        // Only `margin` is physically held now — the open-order requirement is derived and
        // was never taken out of the wallet, so there is no reservation term to add back.
        s += p.margin as i128;
        let mv = calc_value(PRICE, p.amount.unsigned_abs(), 8, 9).unwrap() as i128;
        s += if p.amount >= 0 { mv } else { -mv };
        s += p.v_quote_balance as i128;
    }
    s += storage::load_insurance_fund(ctx).unwrap() as i128;
    s += market_fee_total(ctx) as i128;
    s += storage::load_account(ctx, ADMIN)
        .unwrap()
        .perp_wallet_balance as i128;
    s
}

/// One order through the FULL entry point, EVM-framed exactly like on-chain: checkpoint,
/// run, commit on success / revert on reverted-or-error, then commit_tx (tx boundary).
/// Returns the order id on a successful place, and adds the call's self-trade-prevention
/// cancels to `stp_cancels` (read before `commit_tx`, which clears the logs).
fn framed_place(
    ctx: &mut TestCtx,
    c: Address,
    side: u8,
    price: u64,
    qty: u64,
    tif: u8,
    stp_cancels: &mut u32,
) -> Option<[u8; 32]> {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: 0,
        tif,
        clientOrderId: FixedBytes::default(),
                flags: 0,
    }
    .abi_encode();
    let cp = ctx.journal_mut().checkpoint();
    let out = run_perp_dex_call(&input, 10_000_000, c, U256::ZERO, false, ctx)
        .expect("must not hard-fail");
    let id = if out.reverted {
        ctx.journal_mut().checkpoint_revert(cp);
        None
    } else {
        ctx.journal_mut().checkpoint_commit();
        Some(out.bytes[..32].try_into().unwrap())
    };
    *stp_cancels += stp_cancel_count(&JournalTr::take_logs(ctx.journal_mut()));
    ctx.journal_mut().commit_tx();
    id
}

fn framed_cancel(ctx: &mut TestCtx, c: Address, oid: [u8; 32]) {
    let input = cancelOrderCall {
        orderId: oid.into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    let cp = ctx.journal_mut().checkpoint();
    let out = run_perp_dex_call(&input, 10_000_000, c, U256::ZERO, false, ctx)
        .expect("must not hard-fail");
    if out.reverted {
        ctx.journal_mut().checkpoint_revert(cp);
    } else {
        ctx.journal_mut().checkpoint_commit();
    }
    ctx.journal_mut().commit_tx();
}

#[test]
fn realistic_mix_conserves_equity() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    for i in 0..N_ACCT {
        fund(&mut ctx, addr(i), ACCT_WALLET);
    }
    let initial = total_equity(&mut ctx);

    let mut s: u64 = 0x243F6A8885A308D3;
    let mut rng = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut resting: Vec<Option<[u8; 32]>> = vec![None; N_ACCT as usize];
    let mut stp_cancels = 0u32;

    for op in 0..6000u32 {
        let ai = (rng() % N_ACCT) as usize;
        let c = addr(ai as u64);
        // Probabilistic requote (30%): otherwise let orders REST to build a deep multi-level book.
        if resting[ai].is_some() && rng() % 100 < 30 {
            framed_cancel(&mut ctx, c, resting[ai].take().unwrap());
        } else {
            let roll = rng() % 100;
            let qty = QTY * (1 + rng() % 8);
            // Wide bands (±6 ticks) → many distinct price levels → multi-level sweeps.
            let placed = if roll < 25 {
                let price = PRICE - TICK * (1 + rng() % 6);
                framed_place(&mut ctx, c, 0, price, qty, 3, &mut stp_cancels)
            } else if roll < 50 {
                let price = PRICE + TICK * (1 + rng() % 6);
                framed_place(&mut ctx, c, 1, price, qty, 3, &mut stp_cancels)
            } else if roll < 70 {
                // IOC Buy taker crossing the WHOLE ask band (sweeps multiple sell levels)
                framed_place(&mut ctx, c, 0, PRICE + TICK * 7, qty, 1, &mut stp_cancels)
            } else if roll < 85 {
                framed_place(&mut ctx, c, 1, PRICE - TICK * 7, qty, 1, &mut stp_cancels)
            } else if roll < 95 {
                let side = (rng() % 2) as u8;
                let price = if side == 0 {
                    PRICE + TICK * (1 + rng() % 6)
                } else {
                    PRICE - TICK * (1 + rng() % 6)
                };
                framed_place(&mut ctx, c, side, price, qty, 0, &mut stp_cancels)
            } else {
                let side = (rng() % 2) as u8;
                let price = if side == 0 {
                    PRICE + TICK * 7
                } else {
                    PRICE - TICK * 7
                };
                framed_place(&mut ctx, c, side, price, qty, 2, &mut stp_cancels)
            };
            // Remember any order that may have rested (maker sides + non-crossing GTC).
            if let Some(id) = placed {
                resting[ai] = Some(id);
            }
        }
        // Per-op conservation: total system equity is invariant (no funding / no mark move /
        // no bad debt). A residual write-then-error leaks value and breaks this immediately.
        let now = total_equity(&mut ctx);
        assert_eq!(
            now,
            initial,
            "equity drifted by {} at op {op}",
            now - initial
        );
    }
    // Every account both rests and takes, so its GTC/IOC takers meet its own resting orders:
    // the pass exercises self-trade prevention's cancels under the same conservation check.
    println!("realistic mix: {stp_cancels} STP cancels");
    assert!(stp_cancels >= STP_FLOOR_CONSERVATION, "too few STP cancels ({stp_cancels})");
}
