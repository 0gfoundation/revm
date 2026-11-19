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
            // Funding OFF: it is not what is under test here, and it would turn the identity
            // into an inequality. Each position settles against the index on its own touch and
            // rounds its own payment toward −∞ (`math::calc_funding_payment`), so the two sides
            // need not net to zero: funding only ever BURNS dust, leaving `custody ≥ claims`.
            // That is pinned on its own grid by
            // `funding_rounding_burns_dust_into_custody_and_never_mints` below.
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
    claims_of(ctx, &HOLDERS, clamp_wallets)
}

/// [`claims`] over an explicit holder set (every account of the closed system, ADMIN included).
fn claims_of(ctx: &mut TestCtx, holders: &[Address], clamp_wallets: bool) -> Claims {
    let mut c = Claims {
        insurance: storage::load_insurance_fund(ctx).unwrap() as i128,
        ..Claims::default()
    };
    for &user in holders {
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

// ── Funding rounding: dust is BURNED into custody, never minted (review Q5) ─────────────────────

/// Longs of 0.001 / 0.001 / 0.001 / 0.002 / 0.003 and shorts of 0.003 / 0.005 on the funding grid.
const FL1: Address = address!("00000000000000000000000000000000000000f1");
const FL2: Address = address!("00000000000000000000000000000000000000f2");
const FL3: Address = address!("00000000000000000000000000000000000000f3");
const FL4: Address = address!("00000000000000000000000000000000000000f4");
const FL5: Address = address!("00000000000000000000000000000000000000f5");
const FS1: Address = address!("00000000000000000000000000000000000000f6");
const FS2: Address = address!("00000000000000000000000000000000000000f7");
const TRADERS: [Address; 7] = [FL1, FL2, FL3, FL4, FL5, FS1, FS2];
/// The closed system: every trader, plus ADMIN (the fee sink).
const FUNDING_HOLDERS: [Address; 8] = [FL1, FL2, FL3, FL4, FL5, FS1, FS2, ADMIN];
const FUNDING_DEPOSIT: u64 = 10_000_000; // $10 each
const FUNDING_CUSTODY: i128 = FUNDING_DEPOSIT as i128 * TRADERS.len() as i128;
/// $100.00 — the index every tick publishes, and (with the last trade there too) the mark.
const F_MARK: u64 = 10_000;
/// `interestRate`: at `mark == index` the premium is 0 and the rate is the interest-rate clamp.
const F_RATE: i64 = 6;
/// The cumulative index's step per epoch, `mark · rate`.
const F_STEP: i128 = F_MARK as i128 * F_RATE as i128;
/// `calc_funding_payment` on `(base_decimals 3, price_decimals 2)`: `payment = floor(−amount · Δ
/// / 10^5)`, so one epoch makes 0.001 base owe exactly 0.6 quote units.
const F_SCALE: i128 = 100_000;

/// A 3 / 2-decimal market at $100.00 with an epoch every 60 s at a 6 ppm rate, so no funding
/// payment on it is a whole number of units unless the size makes it one. Fixture, not money:
/// every unit of collateral arrives through `deposit`.
fn setup_funding_market(ctx: &mut TestCtx) {
    storage::save_admin(ctx, ADMIN).unwrap();
    storage::save_market(
        ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 3,
            price_decimals: PRICE_DECIMALS,
            tick_size: 1,
            step_size: 1,
            min_quantity: 1,
            max_quantity: 1_000_000,
            max_price: 1_000_000,
            price_update_interval: 15,
            active: true,
            funding_interval: 60,
            interest_rate: F_RATE,
            liquidation_fee_rate_bps: 0,
            price_band_bps: 0,
            mark_price: 0,
            tiers: MarginTiers::default(),
            basis_mode: 0,
            deferred_work: 0,
            cumulative_funding_index: 0,
        },
    )
    .unwrap();
    storage::save_mark_price(ctx, MARKET_ID, F_MARK).unwrap();
}

/// The test's own ledger of every position's funding, and of what each settlement paid under the
/// rule in force (floor, toward −∞) and under the one it replaced (truncation toward 0).
#[derive(Default)]
struct FundingLedger {
    /// `(holder, amount, last_funding_index)`.
    rows: Vec<(Address, i64, i128)>,
    /// Σ payments under the floor rule, and under truncation.
    floor_sum: i128,
    trunc_sum: i128,
    /// Non-zero settlements; those that were not a whole number of units; and of those, the
    /// PAYERS' (where the two rules differ, by exactly one unit).
    settlements: u32,
    non_exact: u32,
    non_exact_payers: u32,
}

impl FundingLedger {
    fn row(&mut self, user: Address) -> &mut (Address, i64, i128) {
        if let Some(i) = self.rows.iter().position(|r| r.0 == user) {
            return &mut self.rows[i];
        }
        self.rows.push((user, 0, 0));
        self.rows.last_mut().unwrap()
    }

    /// What settling `user` at `index` would pay, under the floor rule, without settling it.
    fn pending(&self, user: Address, index: i128) -> i64 {
        let (amount, anchor) =
            self.rows.iter().find(|r| r.0 == user).map_or((0, 0), |r| (r.1, r.2));
        i64::try_from((-(amount as i128) * (index - anchor)).div_euclid(F_SCALE)).unwrap()
    }

    /// Settles `user` at `index`: the payment the engine must publish (`0` = no event).
    fn settle(&mut self, user: Address, index: i128) -> i64 {
        let (_, amount, anchor) = *self.row(user);
        let exact_scaled = -(amount as i128) * (index - anchor); // exact · F_SCALE
        let floor = exact_scaled.div_euclid(F_SCALE);
        let trunc = exact_scaled / F_SCALE;
        self.row(user).2 = index;
        if floor != 0 {
            self.settlements += 1;
        }
        if exact_scaled % F_SCALE != 0 {
            self.non_exact += 1;
            if exact_scaled < 0 {
                self.non_exact_payers += 1;
                assert_eq!(floor, trunc - 1, "a non-exact payer pays one unit more than before");
            }
        }
        self.floor_sum += floor;
        self.trunc_sum += trunc;
        i64::try_from(floor).unwrap()
    }

    fn fill(&mut self, user: Address, delta: i64) {
        self.row(user).1 += delta;
    }
}

fn funding_index(ctx: &mut TestCtx) -> i128 {
    storage::load_funding_state(ctx, MARKET_ID).unwrap().cumulative_funding_index
}

/// The logs since the last take must carry exactly the `FundingSettled` the ledger expects for
/// `touched` (settled at the current index), and none for anyone else.
fn assert_funding_events(ctx: &mut TestCtx, ledger: &mut FundingLedger, touched: &[Address]) {
    let index = funding_index(ctx);
    let logs = JournalTr::take_logs(ctx.journal_mut());
    let settled = decode_events::<crate::interface::IPerpDex::FundingSettled>(&logs);
    for &user in touched {
        let expected = ledger.settle(user, index);
        let got: Vec<i64> = settled.iter().filter(|f| f.user == user).map(|f| f.amount).collect();
        let want = if expected == 0 { vec![] } else { vec![expected] };
        assert_eq!(got, want, "{user}'s FundingSettled at index {index}");
    }
    for f in &settled {
        assert!(touched.contains(&f.user), "funding settled for {}, whom nobody touched", f.user);
    }
}

/// `addPositionMargin(1)`: a public touch that settles the caller's funding, and moves one unit
/// wallet → silo (inside the identity).
fn touch(ctx: &mut TestCtx, ledger: &mut FundingLedger, user: Address) {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    run_add_position_margin(
        &addPositionMarginCall { marketId: MARKET_ID, amount: 1 }.abi_encode(),
        user,
        ctx,
    )
    .unwrap();
    assert_funding_events(ctx, ledger, &[user]);
}

/// `maker` rests `qty` on `maker_side` at $100, `taker` takes it whole: both settle funding.
fn trade(
    ctx: &mut TestCtx,
    ledger: &mut FundingLedger,
    maker: Address,
    maker_side: u8,
    taker: Address,
    qty: u64,
) {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    place_order(ctx, maker, maker_side, F_MARK, qty);
    place_order(ctx, taker, 1 - maker_side, F_MARK, qty);
    assert_funding_events(ctx, ledger, &[maker, taker]);
    let q = qty as i64;
    let (m, t) = if maker_side == BUY { (q, -q) } else { (-q, q) };
    ledger.fill(maker, m);
    ledger.fill(taker, t);
}

/// One funding epoch through the oracle: the tick at the boundary steps the index by `F_STEP`
/// and leaves the mark at $100.00.
fn funding_epoch(ctx: &mut TestCtx, ts: u64) {
    let before = funding_index(ctx);
    oracle_tick(ctx, F_MARK, ts);
    assert_eq!(storage::load_mark_price(ctx, MARKET_ID).unwrap(), F_MARK);
    assert_eq!(funding_index(ctx), before + F_STEP, "one epoch at {ts}");
}

/// With every position settled at the current index: positions and anchors match the ledger,
/// and `custody − claims` is what funding burned. Returns that gap.
fn funding_checkpoint(ctx: &mut TestCtx, ledger: &FundingLedger, at: &str) -> i128 {
    funding_checkpoint_of(ctx, ledger, &FUNDING_HOLDERS, FUNDING_CUSTODY, at)
}

/// [`funding_checkpoint`] over an explicit closed system: its holders and the custody they put
/// in. A FLAT row (the cross variant's touch counterparty between touches) owes nothing whatever
/// its stored anchor, so only its size is checked.
fn funding_checkpoint_of(
    ctx: &mut TestCtx,
    ledger: &FundingLedger,
    holders: &[Address],
    custody: i128,
    at: &str,
) -> i128 {
    let index = funding_index(ctx);
    for &(user, amount, anchor) in &ledger.rows {
        let p = storage::load_position(ctx, user, MARKET_ID).unwrap();
        if amount == 0 {
            assert_eq!(p.amount, 0, "{at}: {user} is flat");
            continue;
        }
        assert_eq!((p.amount, p.last_funding_index), (amount, anchor), "{at}: {user}");
        assert_eq!(anchor, index, "{at}: {user} is not settled — the identity needs everyone");
    }
    let c = claims_of(ctx, holders, false);
    assert_eq!(c.amount, 0, "{at}: net open interest is flat");
    assert_eq!(custodied_usdc(ctx), custody, "{at}: custody moves only on deposit");
    let gap = custody - c.total_at(F_MARK);
    // The whole story of the gap: funding is the only non-conserving step, and the exact
    // payments sum to zero (`Σ amount = 0` over every interval), so the gap is minus the sum of
    // the rounded ones.
    assert_eq!(gap, -ledger.floor_sum, "{at}: custody − claims is exactly the burned dust");
    assert!(gap >= 0, "{at}: CLAIMS EXCEED CUSTODY by {} — funding minted", -gap);
    // Each non-exact settlement is off by less than one unit, so the burn is bounded by them…
    assert!(gap < ledger.non_exact.max(1) as i128, "{at}: {gap} vs {}", ledger.non_exact);
    // …and the old rule is the same settlements with each non-exact PAYER one unit lighter.
    assert_eq!(
        gap - -ledger.trunc_sum,
        ledger.non_exact_payers as i128,
        "{at}: the rules differ by exactly one unit per non-exact payer settlement"
    );
    gap
}

/// **Funding on a non-divisible grid only BURNS dust: `custody ≥ claims`, never the reverse**
/// (review Q5, owner 2026-11-19).
///
/// Five longs and two shorts of uneven sizes on a 3 / 2-decimal market, at a 6 ppm rate: one
/// epoch makes 0.001 base owe exactly 0.6 quote units, so almost no settlement is a whole number
/// of units. Every action is a public call, driven through its per-selector handler —
/// `run_deposit` + `run_transfer_to_perp`, `run_place_order`, `run_update_index_price` (five
/// epochs), and settlement by touch: `run_add_position_margin(1)` or a fill — not through
/// `run_perp_dex_call`'s shell, which adds only the gas / depth gate, the revert and the snapshot
/// flush (every call here succeeds). The positions are ISOLATED; the CROSSED default has its own
/// test below.
/// Some positions settle every epoch, some across two at once (a single floor over the summed
/// index delta), and two positions trade (one flips) between epochs.
///
/// The test keeps its own ledger of every settlement (`FundingLedger`), asserts each
/// `FundingSettled` against it, and at every checkpoint where all positions are settled asserts
/// the custody identity: `custody − claims` equals the dust burned, is `≥ 0`, is below the number
/// of non-exact settlements, and exceeds the old rule's by exactly the number of non-exact payer
/// settlements.
///
/// | checkpoint | settlements (non-exact / of them payers) | burned now | old rule |
/// |---|---|---|---|
/// | A — epoch 1, all touched | 7 (6 / 5) | 3 | −2 (minted 2) |
/// | B — epochs 2–3, a trade + all touched | 16 (14 / 12) | 8 | −4 (minted 4) |
/// | C — epochs 4–5 after a flip, all touched | 23 (20 / 16) | 11 | −5 (minted 5) |
///
/// Measured by running this scenario on the old code (truncation toward 0 for both sides, umi
/// `079bb1ac`, the ledger switched to truncation): every `FundingSettled` matched the truncating
/// ledger, and claims stood at 70_000_002 / 70_000_004 / 70_000_005 against 70_000_000 of custody
/// at A / B / C — `custody − claims` = −2, −4, −5, i.e. 5 µUSDC minted out of nothing.
#[test]
fn funding_rounding_burns_dust_into_custody_and_never_mints() {
    let seeds: Vec<(Address, u64)> = TRADERS.iter().map(|&u| (u, FUNDING_DEPOSIT)).collect();
    let mut db = InMemoryDB::default();
    for (addr, amount) in &seeds {
        db.insert_account_storage(USDC_ADDRESS, erc20_balance_slot(*addr).into(), U256::from(*amount))
            .unwrap();
    }
    crate::margin::margin_type::test_gate::isolated_by_default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS].into_iter().chain(FUNDING_HOLDERS) {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    setup_funding_market(&mut ctx);
    for user in TRADERS {
        fund_perp_wallet(&mut ctx, user, FUNDING_DEPOSIT);
    }
    let mut ledger = FundingLedger::default();

    // ── Positions, all at $100 and index 0: longs 1, 1, 1, 2, 3 against shorts 3, 5 ──
    trade(&mut ctx, &mut ledger, FS1, SELL, FL1, 1);
    trade(&mut ctx, &mut ledger, FS1, SELL, FL2, 1);
    trade(&mut ctx, &mut ledger, FS1, SELL, FL3, 1);
    trade(&mut ctx, &mut ledger, FS2, SELL, FL4, 2);
    trade(&mut ctx, &mut ledger, FS2, SELL, FL5, 3);
    assert_eq!(funding_checkpoint(&mut ctx, &ledger, "opened"), 0, "nothing to round yet");

    // The oracle's first tick starts the epoch clock (boundary at 90).
    oracle_tick(&mut ctx, F_MARK, 30);
    assert_eq!(funding_index(&mut ctx), 0);

    // ── A: one epoch, everyone touched. Longs owe 0.6 / 0.6 / 0.6 / 1.2 / 1.8 and pay 1, 1, 1,
    //    2, 2; the shorts are owed 1.8 and 3.0 and receive 1 and 3. Σ = −3. (Truncation: the
    //    longs paid 0, 0, 0, 1, 1 — Σ = +2, two units minted.)
    funding_epoch(&mut ctx, 90);
    for user in TRADERS {
        touch(&mut ctx, &mut ledger, user);
    }
    assert_eq!(funding_checkpoint(&mut ctx, &ledger, "A"), 3);
    assert_eq!((ledger.trunc_sum, ledger.settlements, ledger.non_exact), (2, 7, 6));

    // ── B: epoch 2, then FL5 sells 1 to FL1 (both settle: −0.6 → −1, −1.8 → −2); epoch 3, then
    //    everyone: FL1 and FL5 owe one epoch on 2 (−1.2 → −2 each), FL2 / FL3 / FL4 / FS1 / FS2
    //    two epochs on their old size, in ONE floor (−1.2 → −2, −1.2 → −2, −2.4 → −3, +3.6 → 3,
    //    +6.0 → 6).
    funding_epoch(&mut ctx, 150);
    trade(&mut ctx, &mut ledger, FL1, BUY, FL5, 1);
    funding_epoch(&mut ctx, 210);
    for user in TRADERS {
        touch(&mut ctx, &mut ledger, user);
    }
    assert_eq!(funding_checkpoint(&mut ctx, &ledger, "B"), 8);
    assert_eq!((ledger.trunc_sum, ledger.settlements, ledger.non_exact), (4, 16, 14));

    // ── C: FL2 sells 2 to FS1 — FL2 flips to SHORT 1, FS1 covers to short 1 (both already
    //    settled: no funding event). Two epochs, then everyone: the longs FL1 / FL3 / FL4 / FL5
    //    owe 2.4 / 1.2 / 2.4 / 2.4 and pay 3, 2, 3, 3; the shorts FL2 / FS1 / FS2 are owed 1.2 /
    //    1.2 / 6.0 and receive 1, 1, 6.
    trade(&mut ctx, &mut ledger, FS1, BUY, FL2, 2);
    assert_eq!(position(&mut ctx, FL2).amount, -1, "FL2 flipped");
    funding_epoch(&mut ctx, 270);
    funding_epoch(&mut ctx, 330);
    for user in TRADERS {
        touch(&mut ctx, &mut ledger, user);
    }
    assert_eq!(funding_checkpoint(&mut ctx, &ledger, "C"), 11);
    assert_eq!(
        (ledger.trunc_sum, ledger.settlements, ledger.non_exact, ledger.non_exact_payers),
        (5, 23, 20, 16)
    );
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0, "the dust is not the fund's");
}

// ── The same on CROSSED positions (the production default) ──────────────────────────────────────

/// The cross variant's touch counterparty: flat before and after every touch, so it never carries
/// funding of its own.
const FT: Address = address!("00000000000000000000000000000000000000f8");
const CROSS_TRADERS: [Address; 8] = [FL1, FL2, FL3, FL4, FL5, FS1, FS2, FT];
/// The cross variant's closed system: every trader, the touch counterparty, and ADMIN.
const CROSS_FUNDING_HOLDERS: [Address; 9] = [FL1, FL2, FL3, FL4, FL5, FS1, FS2, FT, ADMIN];
const CROSS_FUNDING_CUSTODY: i128 = FUNDING_DEPOSIT as i128 * CROSS_TRADERS.len() as i128;

/// A CROSSED position has no silo to `addPositionMargin` to (-4049), so its touch is a FILL: the
/// trader sells one unit to FT and buys it back at $100. Both settle on the first fill (FT, flat,
/// owes nothing), neither on the second (already anchored at this index); FT is flat again.
fn touch_by_fill(ctx: &mut TestCtx, ledger: &mut FundingLedger, user: Address) {
    trade(ctx, ledger, user, SELL, FT, 1);
    trade(ctx, ledger, FT, SELL, user, 1);
}

/// [`funding_checkpoint_of`] over the cross variant's closed system.
fn cross_checkpoint(ctx: &mut TestCtx, ledger: &FundingLedger, at: &str) -> i128 {
    funding_checkpoint_of(ctx, ledger, &CROSS_FUNDING_HOLDERS, CROSS_FUNDING_CUSTODY, at)
}

/// Every trader's pending funding as `getAccount` prices it — `totalCrossWalletBalance − W`, one
/// CROSSED market each — is the payment its touch is about to publish, and equals `expected`.
fn assert_cross_view_pending(ctx: &mut TestCtx, ledger: &FundingLedger, expected: [i64; 7]) {
    let index = funding_index(ctx);
    let mut got = Vec::new();
    for user in TRADERS {
        let view = getAccountCall::abi_decode_returns(
            &run_get_account(&getAccountCall { user }.abi_encode(), ctx).unwrap(),
        )
        .unwrap();
        let w = storage::load_account(ctx, user).unwrap().perp_wallet_balance;
        let pending = view.totalCrossWalletBalance - w;
        assert_eq!(pending, ledger.pending(user, index), "{user}: the view prices what settles");
        got.push(pending);
    }
    assert_eq!(got, expected);
}

/// **The same burn-only rule on CROSSED positions — the production default — where funding lands
/// in the cross wallet, not a silo; and the view prices exactly what settles** (review Q5).
///
/// The isolated test's grid and positions (longs 1 / 1 / 1 / 2 / 3, shorts 3 / 5, 0.001 units, a 6
/// ppm rate at $100), with every trader CROSSED — by `setMarginType` on the flat market where the
/// thread's default is not already CROSSED — under the cross gate. Settlement is by fill
/// ([`touch_by_fill`]). Before each round of touches, every trader's pending funding as
/// `getAccount` prices it must equal the `FundingSettled` its touch then publishes, both checked
/// against the test's own floor ledger: views and settlement go through one function. At every
/// checkpoint `custody − claims` is the dust burned, `≥ 0`, below the number of non-exact
/// settlements, and above the old (truncating) rule's by one unit per non-exact payer settlement.
///
/// | checkpoint | pending FL1..FS2 (view = `FundingSettled`) | burned now | old rule |
/// |---|---|---|---|
/// | A — one epoch | −1 −1 −1 −2 −2 +1 +3 | 3 | −2 (minted 2) |
/// | B — two more epochs, one floor each | −2 −2 −2 −3 −4 +3 +6 | 7 | −3 (minted 3) |
#[test]
fn cross_funding_rounding_burns_dust_and_the_view_prices_what_settles() {
    use crate::{
        interface::IPerpDex::setMarginTypeCall,
        margin::margin_type::{effective_margin_type, run_set_margin_type},
        types::MarginType,
    };
    let _cross = crate::margin::margin_type::test_gate::enable();
    let mut db = InMemoryDB::default();
    for user in CROSS_TRADERS {
        db.insert_account_storage(
            USDC_ADDRESS,
            erc20_balance_slot(user).into(),
            U256::from(FUNDING_DEPOSIT),
        )
        .unwrap();
    }
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS].into_iter().chain(CROSS_FUNDING_HOLDERS) {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    setup_funding_market(&mut ctx);
    for user in CROSS_TRADERS {
        fund_perp_wallet(&mut ctx, user, FUNDING_DEPOSIT);
        let flat = storage::load_position(&mut ctx, user, MARKET_ID).unwrap();
        if effective_margin_type(&flat) != MarginType::Crossed {
            let input =
                setMarginTypeCall { marketId: MARKET_ID, marginType: MarginType::Crossed.code() }
                    .abi_encode();
            run_set_margin_type(&input, user, &mut ctx).unwrap();
        }
    }
    let mut ledger = FundingLedger::default();

    // ── Positions, all at $100 and index 0 — the isolated test's: longs 1, 1, 1, 2, 3 against
    //    shorts 3, 5 — every one CROSSED, with nothing in a silo.
    trade(&mut ctx, &mut ledger, FS1, SELL, FL1, 1);
    trade(&mut ctx, &mut ledger, FS1, SELL, FL2, 1);
    trade(&mut ctx, &mut ledger, FS1, SELL, FL3, 1);
    trade(&mut ctx, &mut ledger, FS2, SELL, FL4, 2);
    trade(&mut ctx, &mut ledger, FS2, SELL, FL5, 3);
    for user in TRADERS {
        let p = position(&mut ctx, user);
        assert_eq!((effective_margin_type(&p), p.margin), (MarginType::Crossed, 0), "{user}");
    }
    assert_eq!(cross_checkpoint(&mut ctx, &ledger, "opened"), 0, "nothing to round yet");

    oracle_tick(&mut ctx, F_MARK, 30);

    // ── A: one epoch. Longs owe 0.6 / 0.6 / 0.6 / 1.2 / 1.8 and pay 1, 1, 1, 2, 2; the shorts are
    //    owed 1.8 and 3.0 and receive 1 and 3. Σ = −3 (truncation: +2, two units minted).
    funding_epoch(&mut ctx, 90);
    assert_cross_view_pending(&mut ctx, &ledger, [-1, -1, -1, -2, -2, 1, 3]);
    for user in TRADERS {
        touch_by_fill(&mut ctx, &mut ledger, user);
    }
    assert_eq!(cross_checkpoint(&mut ctx, &ledger, "A"), 3);
    assert_eq!((ledger.trunc_sum, ledger.settlements, ledger.non_exact), (2, 7, 6));

    // ── B: two epochs, then everyone, in ONE floor over both: longs owe 1.2 / 1.2 / 1.2 / 2.4 /
    //    3.6 and pay 2, 2, 2, 3, 4; the shorts are owed 3.6 and 6.0 and receive 3 and 6. Σ = −4
    //    (truncation: +1).
    funding_epoch(&mut ctx, 150);
    funding_epoch(&mut ctx, 210);
    assert_cross_view_pending(&mut ctx, &ledger, [-2, -2, -2, -3, -4, 3, 6]);
    for user in TRADERS {
        touch_by_fill(&mut ctx, &mut ledger, user);
    }
    assert_eq!(cross_checkpoint(&mut ctx, &ledger, "B"), 7);
    assert_eq!(
        (ledger.trunc_sum, ledger.settlements, ledger.non_exact, ledger.non_exact_payers),
        (3, 14, 12, 10)
    );
    assert_cross_view_pending(&mut ctx, &ledger, [0; 7]);
    for user in TRADERS {
        let p = position(&mut ctx, user);
        assert_eq!(effective_margin_type(&p), MarginType::Crossed, "{user} reopened CROSSED");
    }
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0, "the dust is not the fund's");
}
