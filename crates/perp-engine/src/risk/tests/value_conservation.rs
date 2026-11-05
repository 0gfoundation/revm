use super::*;

const N: u64 = 8;
const SEED_WALLET: i64 = 400_000_000; // $400 each
const SEED_IF: u64 = 10_000_000_000; // deep enough that bad debt is always covered
/// Coverage floor on self-trade-prevention cancels in the randomised pass — about half of the
/// 41 the seeded pass reaches.
const STP_FLOOR: u32 = 20;

fn user(i: u64) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0x7C;
    b[12..20].copy_from_slice(&i.to_be_bytes());
    Address::from(b)
}

/// `(C, A)`: the price-independent part of the identity, and the net open interest.
/// `C = Σ(wallet + margin + v_quote) + insurance_fund`, `A = Σ amount`.
fn state(ctx: &mut TestCtx) -> (i128, i128) {
    let mut c = storage::load_insurance_fund(ctx).unwrap() as i128;
    let mut a: i128 = 0;
    for i in 0..N {
        let u = user(i);
        let acc = storage::load_account(ctx, u).unwrap();
        let p = storage::load_position(ctx, u, MARKET_ID).unwrap();
        c += acc.perp_wallet_balance as i128 + p.margin as i128 + p.v_quote_balance as i128;
        a += p.amount as i128;
    }
    // ADMIN is the trading-fee sink, so it is part of the closed system.
    c += storage::load_account(ctx, ADMIN)
        .unwrap()
        .perp_wallet_balance as i128;
    (c, a)
}

/// `E(p) = C + signed_value(p, A)`. `base_decimals = 0` and `price_decimals = 2` here, so
/// `calc_value` is exact (`p × q × 10^4`) and the per-user sum equals the value of the sum.
fn equity(c: i128, a: i128, price: u64) -> i128 {
    let v = crate::math::calc_value(price, a.unsigned_abs() as u64, 0, PRICE_DECIMALS).unwrap()
        as i128;
    c + if a >= 0 { v } else { -v }
}

/// Returns how many self-trade-prevention cancels (`OrderCancelled`, reason 9) the frame
/// committed — read before `commit_tx`, which drains the log buffer.
fn framed<F: FnOnce(&mut TestCtx) -> Result<Bytes, PerpError>>(ctx: &mut TestCtx, f: F) -> u32 {
    // EVM-framed exactly like on-chain: a genuine reject reverts the frame and must leave the
    // system value untouched, so rejects are part of what this test covers.
    let cp = ctx.journal_mut().checkpoint();
    match f(ctx) {
        Ok(_) => ctx.journal_mut().checkpoint_commit(),
        Err(PerpError::Reject(_)) => ctx.journal_mut().checkpoint_revert(cp),
        Err(e) => panic!("hard failure: {e:?}"),
    }
    let stp = JournalTr::take_logs(ctx.journal_mut())
        .iter()
        .filter(|l| {
            l.data.topics().first()
                == Some(&crate::interface::IPerpDex::OrderCancelled::SIGNATURE_HASH)
        })
        .filter(|l| {
            crate::interface::IPerpDex::OrderCancelled::decode_raw_log(
                l.data.topics(),
                &l.data.data,
            )
            .unwrap()
            .reason
                == crate::types::CancelReason::SelfTradePrevention as u8
        })
        .count() as u32;
    ctx.journal_mut().commit_tx();
    stp
}

/// Fills, partial fills, cancels, leverage changes, liquidations and ADL, in a randomised
/// order, with the total system value re-checked after EVERY operation.
#[test]
fn a_randomised_operation_sequence_conserves_total_system_value() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // A live clearance fee, so the liquidation → insurance-fund leg is exercised (it is a
    // transfer inside the identity, not an inflow).
    let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    m.liquidation_fee_rate_bps = 50;
    m.price_band_bps = 2_000; // ±20%, wide enough for the mark walk below
    storage::save_market(&mut ctx, &m).unwrap();
    storage::save_insurance_fund(&mut ctx, SEED_IF).unwrap();
    for i in 0..N {
        let u = user(i);
        storage::save_account(
            &mut ctx,
            u,
            UserAccount {
                perp_wallet_balance: SEED_WALLET,
                // Live fee rates: fees must move between users and ADMIN without leaking.
                maker_fee_ppm: 500,
                taker_fee_ppm: 1_000,
                ..UserAccount::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
    }

    let mut s: u64 = 0xC0FFEE_1234_5678;
    let mut rng = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };

    let mut resting: Vec<Option<[u8; 32]>> = vec![None; N as usize];
    let (mut c0, mut a0) = state(&mut ctx);
    let mut index_price: u64 = ENTRY_PRICE;
    let mut ts: u64 = 100;
    let mut liquidations = 0u32;
    let mut fills = 0u32;
    // Wallets that were negative going INTO this step, so a NEW negative can be attributed to
    // the step that produced it (see the gate below).
    let mut was_negative: Vec<bool> = vec![false; N as usize];
    let mut negative_wallet_steps = 0u32;
    let mut stp_cancels = 0u32;

    for op in 0..1200u32 {
        let i = (rng() % N) as usize;
        let who = user(i as u64);
        let roll = rng() % 100;
        let step_can_debit_a_wallet = (20..90).contains(&roll);

        if roll < 12 {
            if let Some(id) = resting[i].take() {
                stp_cancels += framed(&mut ctx, |c| {
                    crate::trading::run_cancel_order(
                        &crate::interface::IPerpDex::cancelOrderCall {
                            orderId: id.into(),
                            marketId: MARKET_ID,
                        }
                        .abi_encode(),
                        who,
                        c,
                    )
                });
            }
        } else if roll < 20 {
            let lev = 1 + rng() % 3;
            stp_cancels += framed(&mut ctx, |c| {
                run_set_leverage(
                    &setLeverageCall {
                        marketId: MARKET_ID,
                        leverage: lev,
                    }
                    .abi_encode(),
                    who,
                    c,
                )
            });
        } else if roll < 90 {
            // Place: passive limits build the book, aggressive limits cross it (fills,
            // partial fills, and the taker cover path when the account is tight).
            let mark = storage::load_mark_price(&mut ctx, MARKET_ID).unwrap();
            let side = (rng() % 2) as u8;
            let aggressive = rng() % 100 < 40;
            let offset = (1 + rng() % 8) as u64 * 100; // $1..$8
            let price = if (side == 0) == aggressive {
                mark.saturating_add(offset)
            } else {
                mark.saturating_sub(offset).max(100)
            };
            let qty = 1 + rng() % 6;
            let before = storage::load_position(&mut ctx, who, MARKET_ID)
                .unwrap()
                .amount;
            let mut id = None;
            stp_cancels += framed(&mut ctx, |c| {
                let r = try_place_order(c, who, side, price, qty);
                if let Ok(bytes) = &r {
                    id = Some(bytes[..32].try_into().unwrap());
                }
                r
            });
            if let Some(id) = id {
                resting[i] = Some(id);
            }
            if storage::load_position(&mut ctx, who, MARKET_ID)
                .unwrap()
                .amount
                != before
            {
                fills += 1;
            }
        } else {
            // Oracle update: moves the mark and runs the liquidation sweep (which runs ADL on
            // any insolvent residual the book cannot absorb). The index is walked as a
            // SAWTOOTH between $60 and $160 rather than a random walk — positions here run at
            // leverage <= 3 against a 1/6 maintenance rate, so only a sustained ~35% adverse
            // move actually puts one under water, and the point of this test is to reach the
            // liquidation / residual-settle / ADL paths, not to wander near the entry price.
            let target: i64 = if (op / 150) % 2 == 0 { 6_000 } else { 16_000 };
            let drift = (target - index_price as i64) / 6;
            let jitter = (rng() % 600) as i64 - 300;
            index_price = (index_price as i64 + drift + jitter).clamp(4_000, 20_000) as u64;
            ts += 16;
            let registry_before = storage::load_position_registry(&mut ctx, MARKET_ID)
                .unwrap()
                .len();
            stp_cancels += framed(&mut ctx, |c| {
                run_update_index_price(
                    &updateIndexPriceCall {
                        marketId: MARKET_ID,
                        indexPrice: index_price,
                        timestamp: ts,
                    }
                    .abi_encode(),
                    ADMIN,
                    c,
                )
            });
            let registry_after = storage::load_position_registry(&mut ctx, MARKET_ID)
                .unwrap()
                .len();
            liquidations += registry_before.saturating_sub(registry_after) as u32;
        }

        // ── The gate ──
        // Both sides evaluated at the POST-operation mark: that is the price any liquidation
        // residual was settled at, and the only price at which the identity closes across it.
        let mark = storage::load_mark_price(&mut ctx, MARKET_ID).unwrap();
        let (c1, a1) = state(&mut ctx);
        assert_eq!(
            equity(c1, a1, mark),
            equity(c0, a0, mark),
            "value leaked at op {op}: {} (mark {mark})",
            equity(c1, a1, mark) - equity(c0, a0, mark)
        );
        // Socialised bad debt would make `E` rise LEGITIMATELY, so the gate above would stop
        // meaning anything. `absorb_from_insurance_fund` can only leave a remainder once the
        // fund is exhausted, so a strictly-positive fund proves no write-off happened. (The
        // events themselves are unreadable here: `commit_tx` drains the journal's log buffer.)
        assert!(
            storage::load_insurance_fund(&mut ctx).unwrap() > 0,
            "insurance fund exhausted at op {op} — E is no longer comparable"
        );
        // ── A negative wallet is NO LONGER an invariant violation ──
        // This used to assert `w >= 0` on the grounds that an underfunded maker fill was
        // REFUSED. It is not any more: the fill happens and the wallet absorbs the shortfall
        // (Binance never sweeps an under-covered lien; see `settle_maker_fill_core`). So the
        // blanket assertion would now be asserting the OLD behaviour.
        //
        // What replaces it is the attribution: a wallet may only CROSS into negative on a step
        // that actually debits one, i.e. a place/fill. Nothing else in the system may produce a
        // deficit — a cancel moves no money, `setLeverage` moves no money, funding settles
        // against `pos.margin` (A1 isolated funding), and liquidation only ever CREDITS the
        // wallet (`apply_position_fill`: a loss beyond the position's own margin routes to the
        // Insurance Fund as bad debt and never debits the user). If a mark move or a
        // liquidation sweep ever produced a negative wallet, that would be a real bug, and this
        // catches it while the relaxed assertion above no longer would.
        //
        // Value conservation itself is unaffected and is still the primary gate above: the
        // deficit is a TRANSFER (wallet down, `pos.margin` up), never a mint.
        for k in 0..N {
            let w = storage::load_account(&mut ctx, user(k))
                .unwrap()
                .perp_wallet_balance;
            let idx = k as usize;
            if w < 0 && !was_negative[idx] {
                negative_wallet_steps += 1;
                assert!(
                    step_can_debit_a_wallet,
                    "user {k} wallet went negative ({w}) at op {op} on a step that debits no \
                     wallet (roll {roll}: cancel / setLeverage / oracle+liquidation)"
                );
            }
            was_negative[idx] = w < 0;
        }
        c0 = c1;
        a0 = a1;
    }

    // The scenario has to actually reach the interesting paths, or it proves nothing.
    println!(
        "conservation scenario: {fills} fills, {liquidations} liquidations, \
         {negative_wallet_steps} wallet-goes-negative steps, {stp_cancels} STP cancels"
    );
    assert!(fills > 100, "too few fills: {fills}");
    assert!(liquidations > 0, "no position was ever liquidated");
    // Every user both rests and crosses, so its takers meet its own orders: self-trade
    // prevention's cancels run under the same value gate.
    assert!(stp_cancels >= STP_FLOOR, "too few STP cancels: {stp_cancels}");
    // ADL is deliberately NOT asserted here: it needs an insolvent residual the book could not
    // absorb AND an opposite-side holder with no resting orders at all, which a book-building
    // fuzz almost never produces. It gets its own deterministic leg below (and
    // `adl_closes_insolvent_residual_against_opposite_holder_conserving_no_if` above covers
    // the mechanics).
}

/// The ADL leg of the same identity: a forced close against a real counterparty at the
/// liquidated position's bankruptcy price must move value between the two participants and
/// create none. Deterministic, because the fuzz above cannot reliably reach it.
#[test]
fn an_adl_forced_close_conserves_total_system_value() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, SEED_IF).unwrap();

    // user(0): 5x long 10 @ $100 (margin $200, vq −$1000) — the position that goes under.
    // user(1): the sole opposite holder, 5x short 10 @ $100, OFF-BOOK (no resting orders),
    //          deeply in profit once the mark crashes. This is the ADL counterparty.
    // (Same shape as `adl_closes_insolvent_residual_against_opposite_holder_conserving_no_if`,
    // re-measured here against the full system identity rather than a two-user sum.)
    for (i, amount, vq) in [(0u64, QTY, -ENTRY_VALUE), (1, -QTY, ENTRY_VALUE)] {
        storage::save_account(
            &mut ctx,
            user(i),
            UserAccount {
                perp_wallet_balance: SEED_WALLET,
                ..UserAccount::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
        storage::save_position(
            &mut ctx,
            user(i),
            MARKET_ID,
            &PerpPosition {
                amount,
                v_quote_balance: vq,
                margin: MARGIN,
                leverage: 5,
                ..crate::margin::margin_type::never_set_position()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
    }

    let (c0, a0) = state(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    // Crash to $75, past the long's $80 bankruptcy price, with an EMPTY book — so the
    // liquidation's market close fills nothing and the whole insolvent residual reaches ADL.
    // Called directly, not through `framed`: `commit_tx` drains the journal's log buffer, and
    // the ADL / depletion events are what this test reads back.
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: 7_500,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    let logs = JournalTr::take_logs(ctx.journal_mut());
    let adls = logs
        .iter()
        .filter(|l| {
            l.data.topics().first() == Some(&crate::interface::IPerpDex::Adl::SIGNATURE_HASH)
        })
        .count();
    let depleted = logs
        .iter()
        .filter(|l| {
            l.data.topics().first()
                == Some(&crate::interface::IPerpDex::InsuranceFundDepleted::SIGNATURE_HASH)
        })
        .count();
    assert!(adls > 0, "the residual did not reach ADL");
    assert_eq!(
        depleted, 0,
        "no socialised write-off — E must be comparable"
    );

    let mark = storage::load_mark_price(&mut ctx, MARKET_ID).unwrap();
    let (c1, a1) = state(&mut ctx);
    assert_eq!(
        equity(c1, a1, mark),
        equity(c0, a0, mark),
        "ADL leaked {}",
        equity(c1, a1, mark) - equity(c0, a0, mark)
    );
    // ADL is a real trade, so it conserves Σ amount too — unlike a residual settled at mark.
    assert_eq!(a1, a0, "ADL must not mint or burn open interest");
    assert_eq!(
        storage::load_position(&mut ctx, user(0), MARKET_ID)
            .unwrap()
            .amount,
        0,
        "the insolvent residual was fully deleveraged"
    );
}

/// The UNDERFUNDED MAKER FILL leg of the same identity. A maker whose wallet cannot cover the
/// opening margin FILLS anyway (`settle_maker_fill_core`), with the silo funded only to the cash
/// at hand — model **M1**, measured by R11 (`derived-ooim-plan.md` §3a).
///
/// **Why this leg still matters after the M1 switch.** Capping the opening margin is exactly the
/// kind of change that looks conservative and mints: the cap has to move the wallet debit DOWN
/// by the same amount it moves the silo credit down, or value appears/disappears. The identity
/// `C = Σ(wallet + margin + v_quote) + IF` is blind to WHERE the shortfall sits — M1 and M1′
/// conserve equally well, they just split `wallet + margin` differently — so it is the only
/// gate that catches the two halves drifting apart.
///
/// Deterministic, because the fuzz above cannot reliably reach it: its users start with $400
/// each at leverage <= 3, and draining one to within a hair of an opening margin by chance is
/// not something 1200 random ops produce.
///
/// MUTATION-CHECKED, both directions of the M1 cap:
/// * deleting `trial_wallet -= opening_margin` in `settle_maker_fill_core` (what "just let the
///   fill through" naively looks like) leaves the silo funded from nowhere — `equity` gains the
///   whole opening margin;
/// * dropping the `.min((*wallet).max(0) as u64)` cap in `apply_position_fill` (i.e. reverting
///   to M1′) is caught by the two split assertions below, not by the conservation one.
#[test]
fn an_underfunded_maker_fill_conserves_total_system_value() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, SEED_IF).unwrap();

    let maker = user(0);
    let taker = user(1);
    // Short 10 @ $100 at leverage 1 needs exactly $1 000 of opening margin.
    let opening_margin: i64 = 1_000_000_000;
    for (u, w) in [(maker, 2 * opening_margin), (taker, 2 * opening_margin)] {
        storage::save_account(
            &mut ctx,
            u,
            UserAccount {
                perp_wallet_balance: w,
                ..UserAccount::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
    }

    // The sell is admitted while the maker CAN afford it, then the wallet is drained to ONE
    // UNIT short — the production shape (a fee, a funding charge or an adverse mark move
    // between admission and fill).
    place_order(&mut ctx, maker, 1, ENTRY_PRICE, QTY as u64);
    let mut acc = storage::load_account(&mut ctx, maker).unwrap();
    acc.perp_wallet_balance = opening_margin - 1;
    storage::save_account(&mut ctx, maker, acc, AccountUpdateReason::Adjustment).unwrap();

    let mark = storage::load_mark_price(&mut ctx, MARKET_ID).unwrap();
    let (c0, a0) = state(&mut ctx);

    place_order(&mut ctx, taker, 0, ENTRY_PRICE, QTY as u64);

    // The fill happened, the SILO is short by the missing unit, and the wallet is emptied to
    // exactly zero: `opening_margin = min(1_000_000_000, 999_999_999)`.
    let maker_pos = storage::load_position(&mut ctx, maker, MARKET_ID).unwrap();
    assert_eq!(maker_pos.amount, -QTY, "the short was opened");
    assert_eq!(
        maker_pos.margin,
        opening_margin - 1,
        "silo funded to the cash at hand and no further — short by the 1 unit (M1)"
    );
    assert_eq!(
        storage::load_account(&mut ctx, maker)
            .unwrap()
            .perp_wallet_balance,
        0,
        "the wallet is emptied, not driven negative (M1′ would read −1 here)"
    );

    // ── The gate: a shortfall is a TRANSFER, not a mint ──
    let (c1, a1) = state(&mut ctx);
    assert_eq!(
        equity(c1, a1, mark),
        equity(c0, a0, mark),
        "underfunded maker fill leaked {}",
        equity(c1, a1, mark) - equity(c0, a0, mark)
    );
    assert_eq!(a1, a0, "a fill must not mint or burn open interest");
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        SEED_IF,
        "the shortfall is NOT socialised at fill time — it rides on the position, and only \
         reaches the fund if that position later closes insolvent (usdc_custody covers that)"
    );
}
