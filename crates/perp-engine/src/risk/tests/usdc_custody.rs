use super::*;
use crate::{
    account::{
        run_deposit, run_get_account, run_transfer_from_perp, run_transfer_to_perp,
        run_withdraw,
    },
    interface::IPerpDex::{
        depositCall, getAccountCall, getAccountMarginCall, transferFromPerpCall,
        transferToPerpCall, withdrawCall,
    },
    margin::view::run_get_account_margin,
    storage::keys::erc20_balance_slot,
};

/// The maker whose position ends up carrying the deficit.
const MK: Address = address!("00000000000000000000000000000000000000d1");
/// Taker of MK's opening sell.
const T1: Address = address!("00000000000000000000000000000000000000d2");
/// Taker of MK's flipping buy — the fill that leaves MK's silo SHORT.
const T2: Address = address!("00000000000000000000000000000000000000d3");
/// Rests the punitive bid MK's liquidation is forced to close into.
const LQ: Address = address!("00000000000000000000000000000000000000d4");

/// Every account whose claims are part of the closed system. ADMIN is in it: it is the fee sink
/// and the source of the insurance-fund seed.
const HOLDERS: [Address; 5] = [MK, T1, T2, LQ, ADMIN];

const MK_DEPOSIT: u64 = 400_000_000; // $400 — all of MK's own money, ever
const MK_TOP_UP: u64 = 20_000_000; //   $20 deposited AFTER the liquidation
const T1_DEPOSIT: u64 = 2_000_000_000;
const T2_DEPOSIT: u64 = 3_000_000_000;
const LQ_DEPOSIT: u64 = 1_000_000_000;
const IF_SEED: u64 = 1_000_000_000;

const BUY: u8 = 0;
const SELL: u8 = 1;

/// Total USDC that ever enters the DEX in this scenario.
const TOTAL_DEPOSITED: i128 =
    (MK_DEPOSIT + MK_TOP_UP + T1_DEPOSIT + T2_DEPOSIT + LQ_DEPOSIT + IF_SEED) as i128;

fn make_ctx_with_usdc(seeds: &[(Address, u64)]) -> TestCtx {
    let mut db = InMemoryDB::default();
    for (addr, amount) in seeds {
        db.insert_account_storage(
            USDC_ADDRESS,
            erc20_balance_slot(*addr).into(),
            U256::from(*amount),
        )
        .unwrap();
    }
    // Isolated margin, explicitly: every position this suite opens is ISOLATED unless a test
    // switches it (`margin_type::test_gate::isolated_by_default`), under the real gate.
    crate::margin::margin_type::test_gate::isolated_by_default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS]
        .iter()
        .copied()
        .chain(HOLDERS)
    {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

/// The test market. Deliberately NOT `setup_market`: that one seeds two wallets by direct
/// write, which is money from nowhere and would make the custody identity meaningless. Here
/// every unit of collateral arrives through `deposit`.
fn setup_market_no_funding(ctx: &mut TestCtx) {
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
            // Funding OFF: it is not a zero-sum transfer inside the identity (each position
            // settles against the index on its own touch, with its own floored payment), and
            // it is not what is under test here.
            funding_interval: 0,
            interest_rate: 0,
            // LIVE clearance fee, so the liquidation charges one. It also pins
            // `risk/liquidation.rs`'s `.min(account.perp_wallet_balance.max(0))`: MK's wallet is
            // EMPTY when the liquidation lands (the fill drained it into the silo), so the fee must
            // come out as ZERO rather than pushing him under water. Under M1′ this pinned the same
            // clamp against an already-NEGATIVE wallet; the `max(0)` half of it is now covered by
            // `a_maker_close_fee_is_the_only_remaining_way_to_a_negative_wallet` plus the unit
            // tests in `types::account`.
            liquidation_fee_rate_bps: 50,
            // ±50%. Wide, and it has to be: the liquidation close below must reach a bid
            // well under the new mark. Note WHY the default (`0` → ±10%) is not enough —
            // `run_update_index_price` threads the `Market` it loaded BEFORE `save_mark_price`
            // into `run_liquidation_sweep`, so the close's fill-time band is centred on the
            // PRE-update mark, not the one that just triggered the liquidation. At ±10% that
            // makes an in-band bid unreachable after any large move and every close lands in
            // ADL (which never touches the insurance fund), so the IF leg would go untested.
            price_band_bps: 5_000,
            mark_price: 0,
            tiers: MarginTiers::default(), // one tier, max leverage 3, mmr 1/6
            basis_mode: 0,
            deferred_work: 0,
            cumulative_funding_index: 0,
        },
    )
    .unwrap();
    storage::save_mark_price(ctx, MARKET_ID, ENTRY_PRICE).unwrap();
}

/// `deposit` then `transferToPerp` — the only route collateral has into the perp layer.
fn fund_perp_wallet(ctx: &mut TestCtx, user: Address, amount: u64) {
    run_deposit(
        &depositCall {
            amount: U256::from(amount),
        }
        .abi_encode(),
        user,
        ctx,
    )
    .unwrap();
    run_transfer_to_perp(&transferToPerpCall { amount }.abi_encode(), user, ctx).unwrap();
}

/// The protocol's whole liability side, bucket by bucket.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Claims {
    /// Σ `UserAccount::usdc_balance` — the spot/withdrawal leg.
    spot: i128,
    /// Σ `UserAccount::perp_wallet_balance`, **SIGNED**. This is where a receivable lives.
    wallet: i128,
    /// Σ `PerpPosition::margin` — collateral allocated to open positions.
    margin: i128,
    /// The insurance fund.
    insurance: i128,
    /// Σ `PerpPosition::v_quote_balance` — the mark-to-market term's cash half.
    v_quote: i128,
    /// Σ `PerpPosition::amount` — NET open interest.
    amount: i128,
}

impl Claims {
    fn total_at(&self, mark: u64) -> i128 {
        let v =
            crate::math::calc_value(mark, self.amount.unsigned_abs() as u64, 0, PRICE_DECIMALS)
                .unwrap() as i128;
        self.spot
            + self.wallet
            + self.margin
            + self.insurance
            + self.v_quote
            + if self.amount >= 0 { v } else { -v }
    }
}

/// `clamp_wallets` reads each wallet through `visible_perp_wallet_balance()` instead of the
/// stored signed value — i.e. exactly what every `uint64` ABI surface reports.
fn claims(ctx: &mut TestCtx, clamp_wallets: bool) -> Claims {
    let mut c = Claims {
        insurance: storage::load_insurance_fund(ctx).unwrap() as i128,
        ..Claims::default()
    };
    for user in HOLDERS {
        let acc = storage::load_account(ctx, user).unwrap();
        let pos = storage::load_position(ctx, user, MARKET_ID).unwrap();
        let spot: U256 = acc.usdc_balance.clone().into();
        c.spot += spot.to::<u128>() as i128;
        c.wallet += if clamp_wallets {
            acc.visible_perp_wallet_balance() as i128
        } else {
            acc.perp_wallet_balance as i128
        };
        c.margin += pos.margin as i128;
        c.v_quote += pos.v_quote_balance as i128;
        c.amount += pos.amount as i128;
    }
    c
}

/// The USDC the precompile really holds, on-trie.
fn custodied_usdc(ctx: &mut TestCtx) -> i128 {
    storage::load_erc20_balance(ctx, USDC_ADDRESS, PERP_DEX_ADDRESS)
        .unwrap()
        .to::<u128>() as i128
}

/// Assert the custody identity at the current mark, and return the buckets.
fn assert_custody_closes(ctx: &mut TestCtx, expected_custody: i128, at: &str) -> Claims {
    let mark = storage::load_mark_price(ctx, MARKET_ID).unwrap();
    let c = claims(ctx, false);
    let d = custodied_usdc(ctx);
    assert_eq!(
        d, expected_custody,
        "custodied USDC moved unexpectedly at {at}"
    );
    assert_eq!(
        c.amount, 0,
        "net open interest must be flat at {at} (every fill here is two-sided), \
         otherwise the mark-to-market term is not price-independent"
    );
    assert_eq!(
        c.total_at(mark),
        d,
        "CUSTODY BROKEN at {at}: claims {c:?} total {} vs custodied {d} (mark {mark})",
        c.total_at(mark)
    );
    c
}

fn update_index(ctx: &mut TestCtx, index: u64, ts: u64) {
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: index,
            timestamp: ts,
        }
        .abi_encode(),
        ADMIN,
        ctx,
    )
    .unwrap();
}

fn mk_realized_pnl(ctx: &mut TestCtx) -> i128 {
    take_position_changes(ctx)
        .into_iter()
        .filter(|e| e.user == MK)
        .map(|e| e.realizedPnl as i128)
        .sum()
}

/// A maker fill that the wallet cannot fund opens a position whose silo is **SHORT** (model M1),
/// and that position is then liquidated INSOLVENT so the Insurance Fund covers the part the silo
/// could not. Two questions, one scenario:
///
/// 1. **does the custodied USDC still equal the sum of everyone's claims?** (the identity, at
///    every checkpoint — the original purpose of this test, unchanged);
/// 2. **does the shortfall RESOLVE, leaving nothing stranded?** (the structural property that
///    motivated switching M1′ → M1).
///
/// # What moved, and why this scenario is still the right one
///
/// This test was written against **M1′**, where the silo was funded IN FULL and MK's
/// `perp_wallet_balance` went NEGATIVE by the shortfall. It then proved that the negative was a
/// RECEIVABLE rather than a leak (the identity closed with it in as a negative claim, and it was
/// CLAMPING it that broke the identity). That was all true — but a receivable that only a
/// voluntary deposit can clear is a strictly worse structure than one the protocol resolves by
/// itself, and R11 measured that Binance does the latter (`derived-ooim-plan.md` §3a).
///
/// Under **M1** the same scenario, unchanged call for call, produces the deficit INSIDE the
/// position instead: the silo receives the $300 of cash actually at hand against a $366.67
/// requirement, so it is $66.67 short. Nothing else about the fill changes. The consequence
/// shows up at liquidation, and it is the whole point:
///
/// ```text
/// M1′  silo 366.67 → close loses 460.00 → IF absorbs  93.33 ,  wallet stranded at −66.67
/// M1   silo 300.00 → close loses 460.00 → IF absorbs 160.00 ,  wallet at 0
///                                             ↑ +66.67 = exactly the stranded balance
/// ```
///
/// The shortfall travels with the position and is absorbed by the EXISTING
/// liquidation-deficit → insurance-fund path (`absorb_bad_debt_into_insurance_fund`). No new
/// mechanism, no new term in the identity: `Σ margin` is $66.67 lower and `Σ wallet` $66.67
/// higher, and the identity is a sum over both.
///
/// # What the custody identity now covers
///
/// Every checkpoint is kept verbatim, and the identity is EXERCISED THE SAME WAY — it is still
/// asserted across an underfunded maker fill, an insolvent liquidation and a later deposit, and
/// it still has to close against real on-trie USDC. What it no longer *contains* is a negative
/// `wallet` term, because this path no longer produces one. That leg has not been deleted, it has
/// MOVED: the only remaining producer of a negative `perp_wallet_balance` is a maker fee the
/// capped opening margin could not absorb (chiefly a pure close whose proceeds an insolvent close
/// ate), which is commission-sized and is the one place Binance's own wallet goes negative too.
/// It is pinned by `trading::tests::a_maker_close_fee_is_the_only_remaining_way_to_a_negative_wallet`,
/// and the signed field + clamped ABI view are kept for it.
///
/// # ⚠️ Still deliberately hostile to "absorb the shortfall from the Insurance Fund" at FILL time
///
/// Crediting a fill-time shortfall out of the IF keeps the SUM unchanged (IF down, wallet or silo
/// up), so the identity alone would not notice. It remains wrong: it socialises a loss that has
/// not happened yet — the position may well close in profit — and it double-pays, because the IF
/// already absorbs the beyond-silo slice at liquidation. The pinned split at the end (own cash +
/// IF == the whole realized loss, with the IF term equal to the beyond-silo slice and nothing
/// more) fails if anyone wires that up. That is on purpose.
#[test]
fn custodied_usdc_still_equals_all_claims_when_a_short_silo_position_is_liquidated_insolvent() {
    let mut ctx = make_ctx_with_usdc(&[
        (MK, MK_DEPOSIT + MK_TOP_UP),
        (T1, T1_DEPOSIT),
        (T2, T2_DEPOSIT),
        (LQ, LQ_DEPOSIT),
        (ADMIN, IF_SEED),
    ]);
    setup_market_no_funding(&mut ctx);

    // ── 1. All collateral enters through `deposit`, so custody is a MEASURED number ──
    for (user, amount) in [
        (MK, MK_DEPOSIT),
        (T1, T1_DEPOSIT),
        (T2, T2_DEPOSIT),
        (LQ, LQ_DEPOSIT),
        (ADMIN, IF_SEED),
    ] {
        fund_perp_wallet(&mut ctx, user, amount);
    }
    // The insurance fund is NOT written directly (that would be a mint): it is seeded out of
    // the admin's own perp wallet, which is where `run_deposit_insurance_fund` takes it from.
    run_deposit_insurance_fund(
        &depositInsuranceFundCall { amount: IF_SEED }.abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();
    let custody_before_top_up = TOTAL_DEPOSITED - MK_TOP_UP as i128;
    assert_custody_closes(&mut ctx, custody_before_top_up, "after funding");

    // ── 2. MK opens a SHORT 10 @ $100 at leverage 3, as the maker ──
    run_set_leverage(
        &setLeverageCall {
            marketId: MARKET_ID,
            leverage: 3,
        }
        .abi_encode(),
        MK,
        &mut ctx,
    )
    .unwrap();
    place_order(&mut ctx, MK, SELL, ENTRY_PRICE, QTY as u64);
    place_order(&mut ctx, T1, BUY, ENTRY_PRICE, QTY as u64);
    let mk_short = storage::load_position(&mut ctx, MK, MARKET_ID).unwrap();
    assert_eq!((mk_short.amount, mk_short.margin), (-QTY, 333_333_333));
    assert_eq!(
        storage::load_account(&mut ctx, MK)
            .unwrap()
            .perp_wallet_balance,
        66_666_667,
        "MK has spent all but $66.67 of its deposit on the short's own margin"
    );
    assert_custody_closes(&mut ctx, custody_before_top_up, "after MK's short opens");

    // ── 3. Mark to $110 — MK's short is underwater but still above maintenance ──
    update_index(&mut ctx, 11_000, 31);
    let mark = storage::load_mark_price(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(
        mark, 11_000,
        "median(price1, price2, lastTraded) must land on the index"
    );
    assert_custody_closes(
        &mut ctx,
        custody_before_top_up,
        "after the mark rises to $110",
    );

    // ── 4. The FLIP: a resting buy of 2× the short costs ooIM = 0, and fills for $366.67 ──
    // This is the production shape of an underfunded maker fill, and it needs no hand-written
    // state at all. `ooIM = IM − PIM` nets the close a fill would perform, so at
    // `Bid ≈ 2|N|` the two branches of the joint `max()` tie and the order is admitted FREE
    // (`trading/resting.rs`'s buy arm even documents this band: unlike a sell, a resting BUY gets
    // no Assuming-Price uplift, so for us the `ooIM = 0` band is both free AND fillable).
    // The fill then closes the short at a LOSS — so the margin it releases is less than the
    // margin the new long leg needs — and the shortfall stays in the SILO (M1).
    assert_eq!(
        oo_im(&mut ctx, MK),
        0,
        "the flipping buy must be admitted free"
    );
    place_order(&mut ctx, MK, BUY, 11_000, 2 * QTY as u64);
    assert_eq!(oo_im(&mut ctx, MK), 0);
    place_order(&mut ctx, T2, SELL, 11_000, 2 * QTY as u64);

    // Cash at hand for the opening leg = wallet 66_666_667 + released margin 333_333_333
    //   + realized −100_000_000 (short 10 @ $100 closed at $110) = 300_000_000,
    // against a requirement of 1_100_000_000 / 3 = 366_666_666. So
    //   opening_margin = min(366_666_666, 300_000_000) = 300_000_000   ⇒ silo SHORT 66_666_666
    //   wallet         = 300_000_000 − 300_000_000     = 0             ⇒ NOT negative
    const SILO_SHORTFALL: i64 = 66_666_666;
    let mk_long = storage::load_position(&mut ctx, MK, MARKET_ID).unwrap();
    assert_eq!(
        (mk_long.amount, mk_long.v_quote_balance, mk_long.margin),
        (QTY, -1_100_000_000, 300_000_000),
        "the flip filled: short closed, long 10 @ $110 opened, silo funded to the CASH AT \
         HAND — short of its 366_666_666 requirement (M1, not M1′)"
    );
    assert_eq!(
        366_666_666 - mk_long.margin,
        SILO_SHORTFALL,
        "the shortfall is exactly the cash the wallet could not produce"
    );
    assert_eq!(
        storage::load_account(&mut ctx, MK)
            .unwrap()
            .perp_wallet_balance,
        0,
        "the wallet is emptied into the silo and stops there — M1′ read −66_666_666 here"
    );
    // K9 passed on the SHORT silo, which is the honest test: equity at the $110 mark is
    // `300_000_000 − 1_100_000_000 + 1_100_000_000` against a `1_100_000_000 / 6` maintenance
    // requirement. Had the fill been priced against a full silo it would have looked safer than
    // it is — that optimism is what M1 removes.
    let mkt_tiers = storage::load_market(&mut ctx, MARKET_ID)
        .unwrap()
        .unwrap()
        .tiers;
    assert!(
        crate::math::is_above_maintenance_margin(
            &mkt_tiers,
            11_000,
            mk_long.amount,
            mk_long.v_quote_balance,
            mk_long.margin,
            0,
            PRICE_DECIMALS,
        )
        .unwrap(),
        "the short-silo position is solvent at the mark it opened at"
    );
    assert_custody_closes(
        &mut ctx,
        custody_before_top_up,
        "after the short-silo flip fill",
    );

    // ── 5. A punitive resting bid at $64, then crash the mark to $70 ──
    // MK's long is insolvent below $73.33 (its bankruptcy price), so a forced close into a $64
    // bid realizes a loss the margin cannot cover and the Insurance Fund pays the remainder.
    // Closing through the BOOK is what reaches the fund at all — a residual the book cannot
    // absorb goes to ADL instead, which never touches it. $64 is chosen to sit INSIDE the
    // close's fill-time price band (see `price_band_bps` in the market fixture above): a bid
    // outside it is skipped and the close would land in ADL.
    place_order(&mut ctx, LQ, BUY, 6_400, QTY as u64);
    assert_custody_closes(&mut ctx, custody_before_top_up, "after LQ's bid rests");

    let if_before_liq = storage::load_insurance_fund(&mut ctx).unwrap();
    let _ = mk_realized_pnl(&mut ctx); // drain: only the liquidation's own PnL is read below
    update_index(&mut ctx, 7_000, 60);
    assert_eq!(
        storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(),
        7_000
    );

    let mk_after = storage::load_position(&mut ctx, MK, MARKET_ID).unwrap();
    assert_eq!(
        (mk_after.amount, mk_after.margin, mk_after.v_quote_balance),
        (0, 0, 0),
        "the sweep liquidated MK's insolvent long through LQ's bid"
    );
    let liq_realized = mk_realized_pnl(&mut ctx);
    assert_eq!(
        liq_realized, -460_000_000,
        "closed 10 @ $110 entry into a $64 bid"
    );
    let if_after_liq = storage::load_insurance_fund(&mut ctx).unwrap();
    let if_absorbed = (if_before_liq - if_after_liq) as i128;
    // ── THE STRUCTURAL PROPERTY M1 BUYS ──
    // The IF absorbs the loss beyond the silo, and the silo is SHORT, so the fill-time
    // shortfall is absorbed here too — through the pre-existing bad-debt path, with no new
    // mechanism. $460 loss − $300 silo = $160, which is the $93.33 M1′ would have routed PLUS
    // the $66.67 M1′ would have stranded on the wallet forever.
    assert_eq!(
        if_absorbed, 160_000_000,
        "the IF covers the loss BEYOND the silo — and the silo was short, so the fill-time \
         shortfall resolves here"
    );
    assert_eq!(
        if_absorbed - 93_333_334,
        SILO_SHORTFALL as i128,
        "and the extra the IF takes vs the full-silo case IS the shortfall, to the unit"
    );
    assert!(
        if_after_liq > 0,
        "no socialised write-off: the identity must stay comparable"
    );

    // MK ends with NOTHING STRANDED: no position, no silo, and a wallet at exactly zero. The
    // wallet is untouched by the liquidation itself — isolated margin never debits it, and the
    // clearance fee is capped at `perp_wallet_balance.max(0)` = 0 (risk/liquidation.rs). This is
    // the assertion M1′ could not make: there it ended at −66_666_666, with the position that
    // justified the debit gone and nothing in the engine able to clear it.
    assert_eq!(
        storage::load_account(&mut ctx, MK)
            .unwrap()
            .perp_wallet_balance,
        0,
        "no stranded balance: the deficit left with the position it was attached to"
    );

    // ── 6. THE VERDICT: custody still equals the sum of all claims ──
    let c = assert_custody_closes(
        &mut ctx,
        custody_before_top_up,
        "after the insolvent liquidation",
    );

    // ...and, unlike under M1′, the CLAMPED view closes too. Under M1′ this assertion measured
    // the identity being broken by exactly the deficit (`uint64` ABI surfaces read the negative
    // wallet as 0, over-stating claims): that was the mechanical proof the negative term was
    // load-bearing. Under M1 there is no negative term left on this path, so the ledger and
    // everything `getAccount` / `AccountBalanceChanged` report AGREE — which is the stronger
    // statement and the one worth pinning: an operator reading only the clamped ABI can no
    // longer be misled about this scenario. Reintroducing M1′ makes this fail.
    let clamped = claims(&mut ctx, true);
    assert_eq!(
        clamped.total_at(7_000) - custody_before_top_up,
        0,
        "no clamped-vs-stored gap: nothing on this path drives a wallet negative any more"
    );
    assert_eq!(
        clamped, c,
        "and that is because the two views are now identical, not because two errors cancel"
    );

    // ── 7. NO DOUBLE COUNT — the loss splits into two DISJOINT slices ──
    // MK's whole realized loss over both fills, funded by: MK's own deposited cash, and the
    // Insurance Fund. Under M1′ this was a THREE-way split — own cash + receivable + IF — and
    // the receivable term has not vanished, it has MERGED into the IF term (that is the
    // `if_absorbed − 93_333_334 == SILO_SHORTFALL` assertion above). If the IF ever absorbed the
    // shortfall a SECOND time (e.g. someone also "resolves" it at fill time) this over-shoots by
    // the overlap. It is exact, so there is no overlap. (Fees are 0 and funding is off in this
    // market, so realized PnL is MK's entire P&L.)
    let mk_total_realized = liq_realized + -100_000_000; // flip close: short @ $100 → $110
    assert_eq!(
        MK_DEPOSIT as i128 + if_absorbed,
        -mk_total_realized,
        "MK's loss must decompose EXACTLY into own-cash + IF, with a ZERO receivable term"
    );
    assert!(
        if_absorbed < -mk_total_realized,
        "the IF must cover strictly less than the whole loss"
    );
    assert_eq!(
        c.margin,
        claims(&mut ctx, false).margin,
        "sanity: `claims` is a pure read"
    );

    // ── 8. A user at zero is refused money-out; a user who tops up is NOT working off a debt ──
    // Under M1′ this section proved receivable semantics: money-out refused because `available`
    // was negative, and the next deposit swallowed by the deficit. Under M1 the first half still
    // holds (MK is at exactly 0, and `derived_can_afford` refuses every POSITIVE requirement),
    // while the second half INVERTS — and that inversion is the user-visible payoff of the
    // switch: MK's $20 is his to spend, because the shortfall was settled against the insurance
    // fund when the position it belonged to died.
    for (label, r) in [
        (
            "transferFromPerp",
            run_transfer_from_perp(
                &transferFromPerpCall { amount: 1 }.abi_encode(),
                MK,
                &mut ctx,
            ),
        ),
        (
            "withdraw",
            run_withdraw(
                &withdrawCall {
                    amount: U256::from(1u64),
                }
                .abi_encode(),
                MK,
                &mut ctx,
            ),
        ),
    ] {
        assert!(
            r.is_err(),
            "{label} must refuse a user whose perp wallet has nothing available"
        );
    }

    fund_perp_wallet(&mut ctx, MK, MK_TOP_UP);
    assert_eq!(
        storage::load_account(&mut ctx, MK)
            .unwrap()
            .perp_wallet_balance,
        MK_TOP_UP as i64,
        "the deposit lands in full — there is no deficit left for it to net against"
    );
    assert!(
        run_transfer_from_perp(
            &transferFromPerpCall { amount: 1 }.abi_encode(),
            MK,
            &mut ctx,
        )
        .is_ok(),
        "and it is SPENDABLE: under M1′ MK would still be under water here, refused"
    );
    assert_custody_closes(&mut ctx, TOTAL_DEPOSITED, "after MK tops up");

    // ── 9. The two ABI views AGREE, and both are signed ──
    // Both account views are unclamped now: `getAccount` returns the roll-up over the per-user
    // market index, `getAccountMargin` the same arithmetic over a caller list. Under M1′ the
    // clamp on the old `getAccount().availablePerpBalance` was the whole difference between "we
    // carry a receivable" and "we carry an INVISIBLE receivable" — an operator watching only the
    // event stream could not see a deficit accumulate. Under M1 there is nothing to hide on this
    // path, so this asserts the two views land on the same number from the two different market
    // sets (the index holds MARKET_ID, which is what the list below passes).
    let expected = MK_TOP_UP as i64 - 1; // the 1 unit transferred out just above
    let index_view = getAccountCall::abi_decode_returns(
        &run_get_account(&getAccountCall { user: MK }.abi_encode(), &mut ctx).unwrap(),
    )
    .unwrap();
    assert_eq!(
        (
            index_view.totalCrossWalletBalance,
            index_view.availableBalance
        ),
        (expected, expected),
        "getAccount reports the full available balance — nothing is being floored away"
    );
    let signed_view = getAccountMarginCall::abi_decode_returns(
        &run_get_account_margin(
            &getAccountMarginCall {
                user: MK,
                marketIds: vec![MARKET_ID],
            }
            .abi_encode(),
            &mut ctx,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (
            signed_view.totalCrossWalletBalance,
            signed_view.availableBalance
        ),
        (expected, expected),
        "getAccountMargin agrees with the index-driven view (no resting orders, so ooIM = 0)"
    );
}
