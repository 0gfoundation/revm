use super::*;
use crate::events::stream_test_support::{account_update_reasons, stream_shape};
use crate::interface::IPerpDex::{
    getPositionRiskCall, AccountPosition, FundingSettled, Liquidation, PositionChanged,
    PositionMarginAdjusted, Trade,
};
use crate::types::AccountUpdateReason as R;

const G1: &str = crate::risk::REMOVE_MARGIN_G1_REJECT;
const G2: &str = crate::risk::REMOVE_MARGIN_G2_REJECT;
const G3: &str = crate::risk::REMOVE_MARGIN_G3_REJECT;
const G4: &str = crate::risk::REMOVE_MARGIN_G4_REJECT;
const LIQ_REFUSED: &str = "liquidate: position is above maintenance margin";

/// Every cap refusal carries Binance's `-4051` with its exact message (the cross flip, R8),
/// then the term that refused it — four distinct strings, written out here so a reworded
/// constant fails.
#[test]
fn every_cap_refusal_carries_binances_4051_and_names_its_term() {
    let p = "removePositionMargin: -4051 Isolated balance insufficient. ";
    assert_eq!(G1, format!("{p}The amount exceeds the position margin"));
    assert_eq!(G2, format!("{p}The resulting margin would be below maintenance margin"));
    assert_eq!(
        G3,
        format!("{p}The resulting equity would be below the initial margin requirement")
    );
    assert_eq!(G4, format!("{p}The resulting position would be below maintenance margin"));
}

/// `q = 3` on the base-0 / price-2 grid: `n(p) = p · 3e4`, divisible by 1, 2, 3 and 6 at every
/// cent price, so every cap identity below holds to the micro-unit.
const Q: i64 = 3;
/// Notional of `Q` at `ENTRY_PRICE`: `10_000 · 3 · 1e6 / 1e2`.
const N0: i64 = 300_000_000;
/// 10 bps = 1_000 ppm.
const TAKER_PPM: u64 = 1_000;
/// 10 bps of `N0` — only ever compared against the fee READ off the `Trade` event.
const OPEN_FEE: u64 = 300_000;
/// ALICE's perp wallet before a real-fill open.
const FUNDS: i64 = 2_000_000_000;
/// A second market on a fractional grid (`price_decimals = 8`): `n = price · qty / 100`, so the
/// notional is truncated and `PIM` rounds up for real — the property test's second half.
const FRAC_MARKET: u64 = 2;
const FRAC_PRICE_DECIMALS: u32 = 8;

// ── read side: the client's view ─────────────────────────────────────────────────────────

fn risk_row_on(ctx: &mut TestCtx, user: Address, market_id: u64) -> AccountPosition {
    let out = crate::margin::view::run_get_position_risk(
        &getPositionRiskCall {
            user,
            marketId: market_id,
        }
        .abi_encode(),
        ctx,
    )
    .unwrap();
    let row = getPositionRiskCall::abi_decode_returns(&out).unwrap();
    assert_eq!(
        row.isolatedMargin as i128,
        row.isolatedWallet as i128 + row.unrealizedProfit as i128,
        "isolatedMargin is isolatedWallet + unrealizedProfit"
    );
    row
}

fn risk_row(ctx: &mut TestCtx) -> AccountPosition {
    risk_row_on(ctx, ALICE, MARKET_ID)
}

/// The cap a client computes from `getPositionRisk` alone — never from the gate:
/// `max(0, min(isolatedWallet − maintMargin, isolatedMargin − max(positionInitialMargin,
/// maintMargin)))`. Valid only with no funding pending (`getPositionRisk` does not settle).
fn cap_of(row: &AccountPosition) -> u64 {
    let iw = row.isolatedWallet as i128;
    let mm = row.maintMargin as i128;
    let pim = row.positionInitialMargin as i128;
    let equity = row.isolatedMargin as i128;
    u64::try_from((iw - mm).min(equity - pim.max(mm)).max(0)).unwrap()
}

/// HEAD's rule before 2b, on the same row: `amount ≤ iw ∧ iw − amount + uPnL ≥ MM`.
fn old_rule_cap(row: &AccountPosition) -> u64 {
    let iw = row.isolatedWallet as i128;
    let u = row.unrealizedProfit as i128;
    let mm = row.maintMargin as i128;
    u64::try_from(iw.min(iw + u - mm).max(0)).unwrap()
}

/// The gate a request of `amount` must fail, read off the row in the documented order
/// (G1 → G4); `None` when it clears all four.
fn gate_for(row: &AccountPosition, amount: u64) -> Option<&'static str> {
    let a = amount as i128;
    let iw = row.isolatedWallet as i128;
    let u = row.unrealizedProfit as i128;
    let pim = row.positionInitialMargin as i128;
    let mm = row.maintMargin as i128;
    if a > iw {
        Some(G1)
    } else if iw - a < mm {
        Some(G2)
    } else if iw - a + u < pim {
        Some(G3)
    } else if iw - a + u < mm {
        Some(G4)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Regime {
    /// `uPnL < PIM − MM`: the equity term `iw + uPnL − PIM` is the smaller one (spec "第二项").
    Equity,
    /// `uPnL > PIM − MM`: the balance term `iw − MM` is the smaller one (spec "第一项").
    Balance,
    /// `MM > PIM ∧ uPnL < 0`: G4's `iw + uPnL − MM` is below both — the safety net.
    SafetyNet,
}

fn regime_of(row: &AccountPosition) -> Option<Regime> {
    let u = row.unrealizedProfit as i128;
    let pim = row.positionInitialMargin as i128;
    let mm = row.maintMargin as i128;
    if mm > pim && u < 0 {
        Some(Regime::SafetyNet)
    } else if u < pim - mm {
        Some(Regime::Equity)
    } else if u > pim - mm {
        Some(Regime::Balance)
    } else {
        None
    }
}

fn assert_regime(row: &AccountPosition, want: Regime) {
    assert_eq!(
        regime_of(row),
        Some(want),
        "fixture regime: uPnL {} PIM {} MM {}",
        row.unrealizedProfit,
        row.positionInitialMargin,
        row.maintMargin
    );
}

// ── write side: the probes ───────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
struct Snapshot {
    pos: PerpPosition,
    account: UserAccount,
    insurance_fund: u64,
    perp_writes: u64,
}

fn snapshot_on(ctx: &mut TestCtx, user: Address, market_id: u64) -> Snapshot {
    Snapshot {
        pos: storage::load_position(ctx, user, market_id).unwrap(),
        account: storage::load_account(ctx, user).unwrap(),
        insurance_fund: storage::load_insurance_fund(ctx).unwrap(),
        perp_writes: ctx.perp_write_count(),
    }
}

fn decode<E: SolEvent>(logs: &[primitives::Log]) -> Vec<E> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect()
}

fn remove_on(
    ctx: &mut TestCtx,
    user: Address,
    market_id: u64,
    amount: u64,
) -> Result<Bytes, PerpError> {
    run_remove_position_margin(
        &removePositionMarginCall {
            marketId: market_id,
            amount,
        }
        .abi_encode(),
        user,
        ctx,
    )
}

/// Sends `amount` and asserts it is refused WHOLE, by exactly `gate`: position (every stored
/// field, `last_funding_index` included), account, insurance fund and the perp write counter
/// bit-identical, and not one log — no `FundingSettled`, no `PositionMarginAdjusted`, no
/// `PositionChanged`, no `AccountBalanceChanged`. Nothing truncated, nothing partially taken,
/// no funding settled on the way out.
fn assert_refused_whole_on(
    ctx: &mut TestCtx,
    user: Address,
    market_id: u64,
    amount: u64,
    gate: &str,
) {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let before = snapshot_on(ctx, user, market_id);
    let err = remove_on(ctx, user, market_id, amount).unwrap_err();
    assert!(
        matches!(&err, PerpError::Reject(m) if m == gate),
        "removePositionMargin({amount}) must be refused by `{gate}`, got `{err}`"
    );
    assert_eq!(
        snapshot_on(ctx, user, market_id),
        before,
        "a refused removePositionMargin({amount}) must leave everything bit-identical"
    );
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert!(
        logs.is_empty(),
        "a refused removePositionMargin({amount}) emitted {:?}",
        stream_shape(&logs)
    );
}

fn assert_refused_whole(ctx: &mut TestCtx, amount: u64, gate: &str) {
    assert_refused_whole_on(ctx, ALICE, MARKET_ID, amount, gate);
}

/// Sends `amount` and asserts it is honoured EXACTLY: margin −amount, wallet +amount, nothing
/// else about the position moves, and one `PositionMarginAdjusted(delta = −amount)`. Only for
/// states with no funding pending (a settle would move the margin and the index too).
fn assert_withdrawn_exactly_on(ctx: &mut TestCtx, user: Address, market_id: u64, amount: u64) {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let pos0 = storage::load_position(ctx, user, market_id).unwrap();
    let wallet0 = storage::load_account(ctx, user).unwrap().perp_wallet_balance;
    remove_on(ctx, user, market_id, amount).unwrap();
    let pos1 = storage::load_position(ctx, user, market_id).unwrap();
    let wallet1 = storage::load_account(ctx, user).unwrap().perp_wallet_balance;
    let mut want = pos0.clone();
    want.margin = pos0.margin - amount as i64;
    assert_eq!(pos1, want, "only the margin moves, and by exactly {amount}");
    assert_eq!(wallet1, wallet0 + amount as i64, "the wallet gets exactly {amount}");
    let logs = JournalTr::take_logs(ctx.journal_mut());
    let adjusted = decode::<PositionMarginAdjusted>(&logs);
    assert_eq!(adjusted.len(), 1);
    assert_eq!(
        (
            adjusted[0].user,
            adjusted[0].marketId,
            adjusted[0].delta,
            adjusted[0].margin
        ),
        (user, market_id, -(amount as i64), pos1.margin)
    );
}

/// THE BOUNDARY PROBE. `cap + 1` first — refused whole by `over_cap_gate` (and the row itself
/// must name that gate) — then `cap`, honoured exactly. With `cap == 0` only the `1` is sent.
/// Afterwards the cap is 0 and the binding term sits exactly on its bound. Returns the cap.
fn probe_on(ctx: &mut TestCtx, user: Address, market_id: u64, over_cap_gate: &str) -> u64 {
    let row = risk_row_on(ctx, user, market_id);
    let cap = cap_of(&row);
    if cap > 0 {
        assert_eq!(gate_for(&row, cap), None, "the cap {cap} itself clears every gate");
    }
    assert_eq!(
        gate_for(&row, cap + 1),
        Some(over_cap_gate),
        "fixture: the row names a different gate for cap + 1 = {}",
        cap + 1
    );
    assert_refused_whole_on(ctx, user, market_id, cap + 1, over_cap_gate);
    if cap > 0 {
        assert_withdrawn_exactly_on(ctx, user, market_id, cap);
        let after = risk_row_on(ctx, user, market_id);
        assert_eq!(cap_of(&after), 0, "taking the cap leaves nothing withdrawable");
        assert!(after.isolatedWallet as i128 >= after.maintMargin as i128);
        assert!(
            after.isolatedMargin as i128
                >= after.positionInitialMargin.max(after.maintMargin) as i128
        );
    }
    cap
}

fn probe(ctx: &mut TestCtx, over_cap_gate: &str) -> u64 {
    probe_on(ctx, ALICE, MARKET_ID, over_cap_gate)
}

/// T9: `liquidate()` at the same mark is refused, and the refusal writes nothing.
fn assert_not_liquidatable(ctx: &mut TestCtx) {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let before = snapshot_on(ctx, ALICE, MARKET_ID);
    let err = liquidate(ctx, ALICE).unwrap_err();
    assert!(
        matches!(&err, PerpError::Reject(m) if m == LIQ_REFUSED),
        "a position at the cap must not be liquidatable at the same mark: {err}"
    );
    assert_eq!(snapshot_on(ctx, ALICE, MARKET_ID), before);
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
}

// ── fixtures ─────────────────────────────────────────────────────────────────────────────

/// A fresh fixture market with ALICE holding a SYNTHETIC position at `mark`, no funding
/// pending — for the tests where exact control of margin / vQuote / leverage matters more
/// than the route there.
fn seeded(amount: i64, v_quote: i64, margin: i64, leverage: u64, mark: u64) -> TestCtx {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(
        &mut ctx,
        ALICE,
        amount,
        v_quote,
        margin,
        leverage,
        USER_WALLET as i64,
    );
    storage::save_mark_price(&mut ctx, MARKET_ID, mark).unwrap();
    ctx
}

/// Opens ALICE `q` contracts at `ENTRY_PRICE` through a REAL matched fill against MAKER, at
/// leverage `lev` and taker fee `taker_ppm`. Returns the opening fee as READ off the `Trade`
/// event, cross-checked against the silo: `floor(value / L) − fee`.
fn open_via_fill(ctx: &mut TestCtx, long: bool, q: u64, lev: u64, taker_ppm: u64) -> u64 {
    storage::save_account(
        ctx,
        ALICE,
        UserAccount {
            perp_wallet_balance: FUNDS,
            ..UserAccount::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    storage::save_user_fee_rates(
        ctx,
        ALICE,
        UserFeeRates {
            maker_fee_ppm: 0,
            taker_fee_ppm: taker_ppm,
        },
    )
    .unwrap();
    set_leverage(ctx, lev).unwrap();
    let (maker_side, taker_side) = if long {
        (Side::Sell, Side::Buy)
    } else {
        (Side::Buy, Side::Sell)
    };
    place_maker_order(ctx, maker_side as u8, ENTRY_PRICE, q);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    try_place_order(ctx, ALICE, taker_side as u8, ENTRY_PRICE, q).unwrap();
    let trades = decode::<Trade>(&JournalTr::take_logs(ctx.journal_mut()));
    assert_eq!(trades.len(), 1);
    assert_eq!(
        (trades[0].taker, trades[0].price, trades[0].quantity),
        (ALICE, ENTRY_PRICE, q)
    );
    let fee = trades[0].takerFee;
    let value = calc_value(ENTRY_PRICE, q, 0, PRICE_DECIMALS).unwrap() as i64;
    let pos = position(ctx, ALICE);
    assert_eq!(pos.amount, if long { q as i64 } else { -(q as i64) });
    assert_eq!(pos.v_quote_balance, if long { -value } else { value });
    assert_eq!(pos.leverage, lev);
    assert_eq!(
        pos.margin,
        value / lev as i64 - fee as i64,
        "the opening fee comes OUT of the silo"
    );
    fee
}

fn opened(long: bool, q: u64, lev: u64, taker_ppm: u64) -> (TestCtx, u64) {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let fee = open_via_fill(&mut ctx, long, q, lev, taker_ppm);
    (ctx, fee)
}

fn set_mark(ctx: &mut TestCtx, mark: u64) {
    storage::save_mark_price(ctx, MARKET_ID, mark).unwrap();
}

// ── T1 / T2 / T2b / T3 / T4: the two terms on both sides of the mark ─────────────────────

/// T1 (+ T9): a LONG in profit, the EQUITY term binding. Real fill (10 bps), `addPositionMargin(E)`
/// with `E` = the opening margin, mark `1.10·entry` (`1.20·entry` at `L = 1`). Closed form:
/// `cap = E − fee + (n₁ − n₀)(1 − 1/L)`. After taking it the silo sits BELOW `PIM` while equity
/// sits exactly ON it, and the position is not liquidatable.
#[test]
fn t1_long_in_profit_withdraws_down_to_equity_equal_to_pim() {
    for (lev, e, mark, want) in [
        (1u64, 300_000_000u64, 12_000u64, 299_700_000u64),
        (2, 150_000_000, 11_000, 164_700_000),
        (3, 100_000_000, 11_000, 119_700_000),
    ] {
        let (mut ctx, fee) = opened(true, Q as u64, lev, TAKER_PPM);
        assert_eq!(fee, OPEN_FEE);
        add_position_margin(&mut ctx, e).unwrap();
        set_mark(&mut ctx, mark);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::Equity);
        let n1 = mark as i64 * Q * 10_000;
        assert_eq!(
            want as i64,
            e as i64 - fee as i64 + (n1 - N0) - (n1 - N0) / lev as i64,
            "L = {lev}: closed form"
        );
        assert_eq!(cap_of(&row), want, "L = {lev}");
        assert_eq!(probe(&mut ctx, G3), want, "L = {lev}");

        let after = risk_row(&mut ctx);
        assert!(
            after.isolatedWallet < after.positionInitialMargin as i64,
            "L = {lev}: the balance ends BELOW PIM, carried by unrealized profit"
        );
        assert_eq!(
            after.isolatedMargin,
            after.positionInitialMargin as i64,
            "L = {lev}: equity lands exactly ON PIM"
        );
        if lev == 3 {
            assert!(after.positionInitialMargin >= 2 * after.maintMargin, "PIM ≥ 2·MM");
        }
        assert_not_liquidatable(&mut ctx);
    }
}

/// `old + 1`, one past the OLD rule's cap `iw + uPnL − MM` on a losing position: the margin
/// left, `MM − uPnL − 1`, still clears G2, while equity lands on `MM − 1` — below `MM` and below
/// `PIM ≥ MM`. G3 and G4 both fail, and G3, the earlier gate, must be the one named.
fn assert_g3_named_before_g4(ctx: &mut TestCtx, row: &AccountPosition, old: u64) {
    let over = old as i128 + 1;
    assert!(row.unrealizedProfit < 0 && row.positionInitialMargin >= row.maintMargin);
    assert!(
        row.isolatedWallet as i128 - over >= row.maintMargin as i128,
        "old + 1 = {over} leaves the margin ≥ MM: G2 passes"
    );
    assert_eq!(
        row.isolatedMargin as i128 - over,
        row.maintMargin as i128 - 1,
        "old + 1 = {over} leaves equity at MM − 1: G3 and G4 both fail"
    );
    assert_refused_whole(ctx, old + 1, G3);
}

/// T2 [extrapolated cell]: a LONG at a loss, mark `0.90·entry`. Closed form
/// `cap = M − n₀ + n₁ − ⌈n₁/L⌉`; the over-cap request fails G3, and so does the OLD rule's own
/// cap (`min(iw, iw + uPnL − MM)`), which HEAD would have honoured — the loss-side tightening.
/// One past the old cap fails G3 AND G4 with G2 passing, which pins G3 ahead of G4.
#[test]
fn t2_long_at_a_loss_is_capped_by_the_equity_term() {
    for (lev, margin, want, old) in [
        (1u64, 600_000_000i64, 300_000_000u64, 525_000_000u64),
        (2, 300_000_000, 135_000_000, 225_000_000),
        (3, 200_000_000, 80_000_000, 125_000_000),
    ] {
        let mut ctx = seeded(Q, -N0, margin, lev, 9_000);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::Equity);
        assert_eq!(row.unrealizedProfit, -30_000_000);
        assert_eq!(
            want as i64,
            margin - N0 + 270_000_000 - 270_000_000 / lev as i64,
            "L = {lev}: closed form"
        );
        assert_eq!(cap_of(&row), want, "L = {lev}");
        assert_eq!(old_rule_cap(&row), old, "L = {lev}");
        assert!(old > want);
        assert_g3_named_before_g4(&mut ctx, &row, old);
        assert_refused_whole(&mut ctx, old, G3);
        assert_eq!(probe(&mut ctx, G3), want, "L = {lev}");
    }
}

/// T2b: a loss-side state where the OLD rule accepts and 2b refuses with the G3 string. Long 10
/// @ $100, `L = 3`, margin $400, mark $95: `N = 950M`, `uPnL = −50M`, `PIM = ⌈950M/3⌉ =
/// 316_666_667`, `MM = 158_333_333`. New cap `min(241_666_667, 33_333_333) = 33_333_333`; old
/// cap `min(400M, 191_666_667)`. Removing $100 clears both of the old rule's checks on these
/// very fields, and 2b refuses it on the equity term.
#[test]
fn t2b_loss_side_old_rule_accepts_and_2b_refuses_on_equity() {
    let mut ctx = seeded(QTY, -ENTRY_VALUE, 400_000_000, 3, 9_500);
    let row = risk_row(&mut ctx);
    assert_regime(&row, Regime::Equity);
    assert_eq!(row.notional, 950_000_000);
    assert_eq!(row.unrealizedProfit, -50_000_000);
    assert_eq!(row.positionInitialMargin, 316_666_667);
    assert_eq!(row.maintMargin, 158_333_333);
    assert_eq!(cap_of(&row), 33_333_333);
    assert_eq!(old_rule_cap(&row), 191_666_667);

    let amount = 100_000_000u64;
    let (iw, u, mm) = (
        row.isolatedWallet as i128,
        row.unrealizedProfit as i128,
        row.maintMargin as i128,
    );
    assert!(
        amount as i128 <= iw && iw - amount as i128 + u >= mm,
        "the old rule's two checks both pass"
    );
    assert_refused_whole(&mut ctx, amount, G3);
    assert_eq!(probe(&mut ctx, G3), 33_333_333);
}

/// T3 (+ T9): a SHORT in profit releases its `PIM` WITH the mark, at slope `1 + 1/L` — marks
/// `0.95·entry` then `0.90·entry`, each probed from an identical fresh state (same `iw`, same
/// `vQuote`, no funding). `q(m₁ − m₂) = 15_000_000` µ, untruncated, and `n(m₁) ≡ n(m₂) (mod L)`,
/// so the slope is exact to the unit. The cap also exceeds `isolatedMargin − ⌈|vQuote|/L⌉`,
/// which rules out "PIM at the worse of entry and mark". The LONG mirror (`1.05`, `1.10`) runs at
/// slope `1 − 1/L` (`0` at `L = 1`: a ×1 long's cap does not move with the mark).
#[test]
fn t3_profit_releases_pim_with_the_mark_at_the_exact_slope() {
    let dq: i64 = 15_000_000;
    for (lev, margin, want) in [
        (1u64, 400_000_000i64, [130_000_000u64, 160_000_000u64]),
        (2, 300_000_000, [172_500_000, 195_000_000]),
        (3, 200_000_000, [120_000_000, 140_000_000]),
    ] {
        let mut caps = [0u64; 2];
        for (i, mark) in [9_500u64, 9_000].into_iter().enumerate() {
            let mut ctx = seeded(-Q, N0, margin, lev, mark);
            let row = risk_row(&mut ctx);
            assert_regime(&row, Regime::Equity);
            let n = mark as i64 * Q * 10_000;
            assert_eq!(
                want[i] as i64,
                margin + N0 - n - n / lev as i64,
                "short L = {lev} @ {mark}: closed form"
            );
            let worse_of = row.isolatedMargin as i128
                - (row.vQuoteBalance.unsigned_abs() as u128).div_ceil(lev as u128) as i128;
            assert!(
                cap_of(&row) as i128 > worse_of,
                "short L = {lev} @ {mark}: PIM is released with the mark"
            );
            caps[i] = probe(&mut ctx, G3);
            assert_eq!(caps[i], want[i], "short L = {lev} @ {mark}");
            if lev == 3 {
                let after = risk_row(&mut ctx);
                assert!(after.positionInitialMargin >= 2 * after.maintMargin, "PIM ≥ 2·MM");
            }
            assert_not_liquidatable(&mut ctx);
        }
        assert_eq!(
            caps[1] as i64 - caps[0] as i64,
            dq + dq / lev as i64,
            "short L = {lev}: q·(m₁ − m₂)·(1 + 1/L)"
        );
    }
    for (lev, margin, want) in [
        (1u64, 600_000_000i64, [300_000_000u64, 300_000_000u64]),
        (2, 300_000_000, [157_500_000, 165_000_000]),
        (3, 200_000_000, [110_000_000, 120_000_000]),
    ] {
        let mut caps = [0u64; 2];
        for (i, mark) in [10_500u64, 11_000].into_iter().enumerate() {
            let mut ctx = seeded(Q, -N0, margin, lev, mark);
            assert_regime(&risk_row(&mut ctx), Regime::Equity);
            caps[i] = probe(&mut ctx, G3);
            assert_eq!(caps[i], want[i], "long L = {lev} @ {mark}");
            assert_not_liquidatable(&mut ctx);
        }
        assert_eq!(
            caps[1] as i64 - caps[0] as i64,
            dq - dq / lev as i64,
            "long L = {lev}: q·(m₂ − m₁)·(1 − 1/L)"
        );
    }
}

/// T4: a SHORT at a loss, mark `1.10·entry` — T2's mirror. Closed form
/// `cap = M + |vQuote| − n₁ − ⌈n₁/L⌉`; the old rule's cap is refused by G3, and so is one past
/// it, where G4 fails too.
#[test]
fn t4_short_at_a_loss_is_capped_by_the_equity_term() {
    for (lev, margin, want, old) in [
        (1u64, 600_000_000i64, 240_000_000u64, 515_000_000u64),
        (2, 300_000_000, 105_000_000, 215_000_000),
        (3, 200_000_000, 60_000_000, 115_000_000),
    ] {
        let mut ctx = seeded(-Q, N0, margin, lev, 11_000);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::Equity);
        assert_eq!(row.unrealizedProfit, -30_000_000);
        assert_eq!(
            want as i64,
            margin + N0 - 330_000_000 - 330_000_000 / lev as i64,
            "L = {lev}: closed form"
        );
        assert_eq!(cap_of(&row), want, "L = {lev}");
        assert_eq!(old_rule_cap(&row), old, "L = {lev}");
        assert_g3_named_before_g4(&mut ctx, &row, old);
        assert_refused_whole(&mut ctx, old, G3);
        assert_eq!(probe(&mut ctx, G3), want, "L = {lev}");
    }
}

// ── T5 / T14: the fresh open ─────────────────────────────────────────────────────────────

/// T5: a ×1 LONG through a real fill. Its equity term is exactly `iw + vQuote`, independent of
/// the mark. ① no margin added: `−fee` at 0.9 / 1.0 / 1.2 × entry, 1 µ refused by G3 each time;
/// ② `addPositionMargin(E)`: the cap is `E − fee` at all three marks (fee as READ off the fill);
/// ③ a zero-fee open: equity sits exactly on `PIM`, the cap is exactly 0 and 1 µ is refused.
#[test]
fn t5_a_1x_long_cap_is_e_minus_the_opening_fee_at_every_mark() {
    const MARKS: [u64; 3] = [9_000, 10_000, 12_000];
    const E: u64 = 300_000_000;

    let (mut ctx, fee) = opened(true, Q as u64, 1, TAKER_PPM);
    assert_eq!(fee, OPEN_FEE);
    for mark in MARKS {
        set_mark(&mut ctx, mark);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::Equity);
        assert_eq!(
            row.isolatedMargin - row.positionInitialMargin as i64,
            -(fee as i64),
            "@ {mark}: the ×1 long's equity term is iw + vQuote = −fee"
        );
        assert_eq!(probe(&mut ctx, G3), 0, "@ {mark}");
    }

    for mark in MARKS {
        let (mut ctx, fee) = opened(true, Q as u64, 1, TAKER_PPM);
        add_position_margin(&mut ctx, E).unwrap();
        set_mark(&mut ctx, mark);
        assert_regime(&risk_row(&mut ctx), Regime::Equity);
        assert_eq!(probe(&mut ctx, G3), E - fee, "@ {mark}");
        assert_eq!(E - fee, 299_700_000);
    }

    for mark in MARKS {
        let (mut ctx, fee) = opened(true, Q as u64, 1, 0);
        assert_eq!(fee, 0);
        set_mark(&mut ctx, mark);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::Equity);
        assert_eq!(row.isolatedMargin, row.positionInitialMargin as i64, "@ {mark}");
        assert_eq!(probe(&mut ctx, G3), 0, "@ {mark}");
    }
}

/// T14: right after a real-fill open, at any `L`, nothing comes out (equity is one fee below
/// `PIM`). It unlocks only once the move outruns the fee — `q·|m − e|·(1 ∓ 1/L) > fee` — and
/// these are the exact last-locked / first-unlocked ticks on the fixture grid:
/// `cap = |Δn|·(L ∓ 1)/L − fee` with `|Δn| = 3e4·|Δticks|` divisible by `L`. A ×1 long never
/// unlocks (slope 0).
#[test]
fn t14_a_fresh_open_unlocks_only_once_the_move_outruns_the_fee() {
    for (long, lev, locked, unlocked, want) in [
        (true, 2u64, 10_020u64, 10_021u64, 15_000u64),
        (true, 3, 10_015, 10_016, 20_000),
        (false, 1, 9_995, 9_994, 60_000),
        (false, 2, 9_994, 9_993, 15_000),
        (false, 3, 9_993, 9_992, 20_000),
    ] {
        let (mut ctx, fee) = opened(long, Q as u64, lev, TAKER_PPM);
        assert_eq!(fee, OPEN_FEE);
        let row = risk_row(&mut ctx);
        assert_eq!(
            row.isolatedMargin - row.positionInitialMargin as i64,
            -(fee as i64),
            "{long}/{lev}: at the open, equity is one fee below PIM"
        );
        assert_eq!(probe(&mut ctx, G3), 0);
        for (mark, cap) in [(locked, 0u64), (unlocked, want)] {
            set_mark(&mut ctx, mark);
            let row = risk_row(&mut ctx);
            assert_regime(&row, Regime::Equity);
            let dn = (mark as i64 - ENTRY_PRICE as i64).abs() * Q * 10_000;
            let slope = if long { lev as i64 - 1 } else { lev as i64 + 1 };
            assert_eq!(dn % lev as i64, 0);
            assert_eq!(
                cap as i64,
                (dn * slope / lev as i64 - fee as i64).max(0),
                "{long}/{lev} @ {mark}: closed form"
            );
            assert_eq!(probe(&mut ctx, G3), cap, "{long}/{lev} @ {mark}");
        }
    }
    let (mut ctx, _) = opened(true, Q as u64, 1, TAKER_PPM);
    for mark in [10_016u64, 15_000, 20_000] {
        set_mark(&mut ctx, mark);
        assert_regime(&risk_row(&mut ctx), Regime::Equity);
        assert_eq!(probe(&mut ctx, G3), 0, "a ×1 long never unlocks @ {mark}");
    }
}

// ── T6: the balance term [extrapolated] ──────────────────────────────────────────────────

/// T6 (+ T9) [extrapolated]: the BALANCE term binds once `uPnL > PIM − MM`. Long `L = 3` @ 1.50
/// and short `L = 3` @ 0.80: `cap + 1` is refused by G2 while its equity would still clear `PIM`
/// — the balance alone refuses it. A fresh `L = 3` long with nothing added has cap 0 at 2.0;
/// its cap peaks at 1.20, where the two terms are exactly equal (and G2, the first gate,
/// names the refusal); just below it (1.19) G3 binds, just above it (1.21) G2.
#[test]
fn t6_the_balance_term_binds_past_pim_minus_mm() {
    for (amount, v_quote, mark, want) in [
        (Q, -N0, 15_000u64, 125_000_000u64),
        (-Q, N0, 8_000, 160_000_000),
    ] {
        let mut ctx = seeded(amount, v_quote, 200_000_000, 3, mark);
        let row = risk_row(&mut ctx);
        assert_regime(&row, Regime::Balance);
        assert_eq!(
            want as i64,
            200_000_000 - mark as i64 * Q * 10_000 / 6,
            "@ {mark}: iw − MM"
        );
        let over = want as i128 + 1;
        assert!(
            row.isolatedWallet as i128 - over + row.unrealizedProfit as i128
                >= row.positionInitialMargin as i128,
            "@ {mark}: cap + 1 would still leave equity ≥ PIM — only the balance refuses it"
        );
        assert_eq!(probe(&mut ctx, G2), want, "@ {mark}");
        assert_not_liquidatable(&mut ctx);
    }

    let (mut ctx, fee) = opened(true, Q as u64, 3, TAKER_PPM);
    assert_eq!(fee, OPEN_FEE);
    set_mark(&mut ctx, 20_000);
    let row = risk_row(&mut ctx);
    assert_regime(&row, Regime::Balance);
    assert_eq!(
        row.isolatedWallet - row.maintMargin as i64,
        -(fee as i64),
        "+100%: iw − MM = −fee"
    );
    assert_eq!(probe(&mut ctx, G2), 0);

    // Either side of the 1.20 peak, each from its own fresh open: at 1.19 the equity term binds
    // (G3), at 1.21 the balance term (G2).
    for (mark, regime, gate, want) in [
        (11_900u64, Regime::Equity, G3, 37_700_000u64),
        (12_100, Regime::Balance, G2, 39_200_000),
    ] {
        let (mut ctx, _) = opened(true, Q as u64, 3, TAKER_PPM);
        set_mark(&mut ctx, mark);
        let row = risk_row(&mut ctx);
        assert_regime(&row, regime);
        assert_eq!(cap_of(&row), want, "@ {mark}: below the 1.20 peak");
        assert_eq!(probe(&mut ctx, gate), want, "@ {mark}");
    }
    let (mut ctx, _) = opened(true, Q as u64, 3, TAKER_PPM);
    set_mark(&mut ctx, 12_000);
    let row = risk_row(&mut ctx);
    let balance_term = row.isolatedWallet as i128 - row.maintMargin as i128;
    let equity_term = row.isolatedMargin as i128 - row.positionInitialMargin as i128;
    assert_eq!(balance_term, equity_term, "1.20·entry: the two terms meet");
    assert_eq!(regime_of(&row), None, "uPnL == PIM − MM exactly");
    assert_eq!(probe(&mut ctx, G2), 39_700_000);
    assert_not_liquidatable(&mut ctx);
}

// ── T7 / T8: whole refusal, and funding ──────────────────────────────────────────────────

/// T7: a refusal is WHOLE — nothing clipped, nothing partially taken, and the pending funding
/// NOT settled (index, margin, insurance fund, logs all untouched). With a 3e6 charge pending
/// the settled silo is 196.7e6 (`PIM` 1e8, `MM` 5e7): 198e6 fits the UNsettled 199.7e6 but is
/// refused by G1, 150e6 by G2, and 96.7e6 + 1 by G3. The full 96.7e6 is still there afterwards.
/// The G3 refusal is also sent through the precompile dispatcher, whose debug-build
/// write-then-revert tripwire fires on any perp write a reverted call leaves behind.
#[test]
fn t7_a_refusal_is_whole_and_settles_no_funding() {
    let mut ctx = seeded(Q, -N0, 199_700_000, 3, ENTRY_PRICE);
    assert_regime(&risk_row(&mut ctx), Regime::Equity);
    set_funding_index(&mut ctx, 100_000_000);
    for (amount, gate) in [
        (i64::MAX as u64, G1),
        (198_000_000u64, G1),
        (150_000_000, G2),
        (96_700_001, G3),
    ] {
        assert_refused_whole(&mut ctx, amount, gate);
        let pos = position(&mut ctx, ALICE);
        assert_eq!(
            (pos.margin, pos.last_funding_index),
            (199_700_000, 0),
            "{amount}: no settle"
        );
    }

    let _ = JournalTr::take_logs(ctx.journal_mut());
    let before = snapshot_on(&mut ctx, ALICE, MARKET_ID);
    let out = run_perp_dex_call(
        &removePositionMarginCall {
            marketId: MARKET_ID,
            amount: 96_700_001,
        }
        .abi_encode(),
        1_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(out.reverted, "an over-cap removePositionMargin must revert");
    assert_eq!(
        <alloy_sol_types::Revert as alloy_sol_types::SolError>::abi_decode(&out.bytes)
            .unwrap()
            .reason,
        G3
    );
    assert_eq!(
        snapshot_on(&mut ctx, ALICE, MARKET_ID),
        before,
        "a reverted call writes nothing and settles no funding"
    );
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());

    let wallet0 = wallet(&mut ctx, ALICE);
    remove_position_margin(&mut ctx, 96_700_000).unwrap();
    let pos = position(&mut ctx, ALICE);
    assert_eq!(pos.margin, 100_000_000, "196.7e6 settled − 96.7e6 = PIM");
    assert_eq!(pos.last_funding_index, 100_000_000);
    assert_eq!(wallet(&mut ctx, ALICE), wallet0 + 96_700_000);
}

/// T7 for G4: a safety-net refusal with funding pending settles nothing either. T15 route ①
/// plus a 1e5 charge (−QTY·Δ/100): settled iw 599_900_000, settled G4 cap 83_233_334, so
/// 83_233_335 clears G1–G3 after settlement (margin 516_666_665 ≥ MM; equity 466_666_665 ≥ PIM)
/// and fails only G4. The unsettled rule would have accepted it (cap 83_333_334). Kills the
/// mutant that settles funding before G4 instead of after every gate: the snapshot stays
/// bit-identical there, so only the empty-logs check of `assert_refused_whole` sees it.
#[test]
fn t7_a_g4_refusal_with_funding_pending_settles_nothing() {
    let mut ctx = seeded(QTY, -1_050_000_000, 600_000_000, 3, ENTRY_PRICE);
    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 100_000_000], &[3, 1]).unwrap();
    let row = risk_row(&mut ctx);
    assert_regime(&row, Regime::SafetyNet);
    assert_eq!(cap_of(&row), 83_333_334);
    set_funding_index(&mut ctx, 1_000_000);
    assert_refused_whole(&mut ctx, 83_233_335, G4);
    assert_eq!(position(&mut ctx, ALICE).last_funding_index, 0);
    remove_position_margin(&mut ctx, 83_233_334).unwrap();
    let pos = position(&mut ctx, ALICE);
    assert_eq!((pos.margin, pos.last_funding_index), (516_666_666, 1_000_000));
    assert_eq!(cap_of(&risk_row(&mut ctx)), 0);
}

/// Shared T8 body: `L = 3` long at the entry mark, silo 199.7e6, `PIM` 1e8, `MM` 5e7 — so the
/// cap BEFORE settlement is 99.7e6 — then a funding index that moves the silo by `funding`.
/// `between` lies between the pre- and post-settlement caps, so the two algorithms disagree on
/// it; the engine must follow the settled one. Returns the post-settlement cap.
fn t8_case(index: i128, funding: i64, between: u64) -> (TestCtx, u64) {
    let mut ctx = seeded(Q, -N0, 199_700_000, 3, ENTRY_PRICE);
    let pre = risk_row(&mut ctx);
    assert_regime(&pre, Regime::Equity);
    let cap_pre = cap_of(&pre);
    assert_eq!(cap_pre, 99_700_000);
    set_funding_index(&mut ctx, index);
    assert_eq!(funding, -(Q as i128 * index / 100) as i64, "−q·Δ/100 on this grid");
    // Both terms move one-for-one with `iw` and neither clamps, so the settled cap is the
    // unsettled one plus the funding.
    let cap_post = (cap_pre as i64 + funding) as u64;
    assert!(
        (cap_pre.min(cap_post)..=cap_pre.max(cap_post)).contains(&between)
            && between != cap_pre
            && between != cap_post
    );
    assert_refused_whole(&mut ctx, cap_post + 1, G3);
    (ctx, cap_post)
}

/// The success stream of a removal that settles funding first: the funding group, then the
/// margin-transfer group — `FundingSettled → … → PositionMarginAdjusted → … PositionChanged`.
fn remove_and_pin_stream(ctx: &mut TestCtx, amount: u64, funding: i64) {
    let pos0 = position(ctx, ALICE);
    let wallet0 = storage::load_account(ctx, ALICE).unwrap().perp_wallet_balance;
    let _ = JournalTr::take_logs(ctx.journal_mut());
    storage::begin_perp_call(ctx);
    remove_position_margin(ctx, amount).unwrap();
    storage::flush_account_snapshots(ctx).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(
        stream_shape(&logs),
        vec![
            ("FundingSettled", Some(ALICE)),
            ("AccountBalanceChanged", Some(ALICE)),
            ("PositionChanged", Some(ALICE)),
            ("PositionMarginAdjusted", Some(ALICE)),
            ("AccountBalanceChanged", Some(ALICE)),
            ("PositionChanged", Some(ALICE)),
        ]
    );
    assert_eq!(
        account_update_reasons(&logs),
        vec![(ALICE, R::FundingFee), (ALICE, R::MarginTransfer)]
    );
    let settled = decode::<FundingSettled>(&logs);
    assert_eq!(settled[0].amount, funding);
    let pos1 = position(ctx, ALICE);
    let adjusted = decode::<PositionMarginAdjusted>(&logs);
    assert_eq!((adjusted[0].delta, adjusted[0].margin), (-(amount as i64), pos1.margin));
    assert_eq!(pos1.margin, pos0.margin + funding - amount as i64);
    assert_eq!(
        storage::load_account(ctx, ALICE).unwrap().perp_wallet_balance,
        wallet0 + amount as i64,
        "funding lands in the silo, never the wallet"
    );
}

/// T8, PAY: a 3e6 charge pending. 98e6 is inside the unsettled cap (99.7e6) and outside the
/// settled one (96.7e6) — refused by G3, nothing settled. The settled cap comes out exactly.
#[test]
fn t8_pending_funding_charge_is_settled_before_the_cap_is_judged() {
    let (mut ctx, cap_post) = t8_case(100_000_000, -3_000_000, 98_000_000);
    assert_eq!(cap_post, 96_700_000);
    assert_refused_whole(&mut ctx, 98_000_000, G3);
    assert_eq!(position(&mut ctx, ALICE).last_funding_index, 0);
    remove_and_pin_stream(&mut ctx, cap_post, -3_000_000);
    let pos = position(&mut ctx, ALICE);
    assert_eq!((pos.margin, pos.last_funding_index), (100_000_000, 100_000_000));
    assert_eq!(cap_of(&risk_row(&mut ctx)), 0);
}

/// T8, RECEIVE: a 3e6 credit pending. 101e6 is outside the unsettled cap (99.7e6) and inside the
/// settled one (102.7e6) — honoured. What is left (1.7e6) is then exactly the next cap.
#[test]
fn t8_pending_funding_credit_is_settled_before_the_cap_is_judged() {
    let (mut ctx, cap_post) = t8_case(-100_000_000, 3_000_000, 101_000_000);
    assert_eq!(cap_post, 102_700_000);
    remove_and_pin_stream(&mut ctx, 101_000_000, 3_000_000);
    let pos = position(&mut ctx, ALICE);
    assert_eq!((pos.margin, pos.last_funding_index), (101_700_000, -100_000_000));
    assert_eq!(probe(&mut ctx, G3), 1_700_000);
}

// ── T10 / T11 / T12 ──────────────────────────────────────────────────────────────────────

/// T10: the precondition errors are unchanged, each refusing whole. The wallet overflow fires
/// only AFTER all four gates pass. (It cannot be combined with pending funding: the funding
/// settle's own `AccountBalanceChanged` payload overflows first on a wallet at `i64::MAX`.)
#[test]
fn t10_precondition_errors_are_unchanged() {
    let mut ctx = seeded(Q, -N0, 199_700_000, 3, ENTRY_PRICE);
    assert_refused_whole(&mut ctx, 0, "removePositionMargin: amount must be > 0");
    for amount in [i64::MAX as u64 + 1, u64::MAX] {
        assert_refused_whole(
            &mut ctx,
            amount,
            "removePositionMargin: amount: value exceeds i64::MAX",
        );
    }
    assert_refused_whole_on(
        &mut ctx,
        ALICE,
        MARKET_ID + 7,
        1,
        "removePositionMargin: unknown market",
    );
    assert_refused_whole_on(
        &mut ctx,
        KEEPER,
        MARKET_ID,
        1,
        "removePositionMargin: no open position",
    );
    set_mark(&mut ctx, 0);
    assert_refused_whole(&mut ctx, 1, "removePositionMargin: mark price unavailable");

    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, Q, -N0, 199_700_000, 3, i64::MAX);
    let row = risk_row(&mut ctx);
    assert_eq!((cap_of(&row), gate_for(&row, 1)), (99_700_000, None), "1 clears every gate");
    assert_refused_whole(&mut ctx, 1, "perp wallet: balance overflow");
}

/// T11: a resting BUY far below the mark (`ooIM` 5e7 > 0) does not move the cap — only the
/// POSITION term `PIM` enters it, never `IM` or `ooIM`. Taking the cap raises
/// `availableBalance` by exactly the amount and leaves `openOrderInitialMargin` unchanged.
#[test]
fn t11_a_resting_order_does_not_move_the_cap() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, Q, -N0, 200_000_000, 3, 1_000_000_000);
    set_mark(&mut ctx, 11_000);
    let before = risk_row(&mut ctx);
    assert_regime(&before, Regime::Equity);
    assert_eq!(before.openOrderInitialMargin, 0);
    let cap_before = cap_of(&before);
    assert_eq!(cap_before, 120_000_000);

    place_order(&mut ctx, ALICE, Side::Buy as u8, 5_000, Q as u64);
    let row = risk_row(&mut ctx);
    assert_eq!(
        row.openOrderInitialMargin, 50_000_000,
        "⌈(3.3e8 + 1.5e8)/3⌉ − 1.1e8"
    );
    assert_eq!(
        (
            row.isolatedWallet,
            row.unrealizedProfit,
            row.positionInitialMargin,
            row.maintMargin
        ),
        (
            before.isolatedWallet,
            before.unrealizedProfit,
            before.positionInitialMargin,
            before.maintMargin
        )
    );
    assert_eq!(cap_of(&row), cap_before, "bit-identical cap with the order resting");

    let available0 = crate::margin::view::derived_available_balance(&mut ctx, ALICE).unwrap();
    assert_eq!(probe(&mut ctx, G3), cap_before);
    let available1 = crate::margin::view::derived_available_balance(&mut ctx, ALICE).unwrap();
    assert_eq!(available1 - available0, cap_before as i128);
    assert_eq!(risk_row(&mut ctx).openOrderInitialMargin, row.openOrderInitialMargin);
}

/// T12: raising leverage lowers `PIM` and raises the cap to the new one. Opened at `L = 1`
/// (real fill, 10 bps): cap 0. At `L = 2`: `PIM` 1.5e8, cap 149.7e6. At `L = 3`: `PIM` 1e8,
/// cap 199.7e6. `setLeverage` moves no margin.
#[test]
fn t12_raising_leverage_raises_the_cap_to_the_new_pim() {
    let (mut ctx, fee) = opened(true, Q as u64, 1, TAKER_PPM);
    assert_eq!(fee, OPEN_FEE);
    let row = risk_row(&mut ctx);
    assert_eq!(row.positionInitialMargin, 300_000_000);
    assert_eq!(cap_of(&row), 0);
    assert_refused_whole(&mut ctx, 1, G3);
    let margin = position(&mut ctx, ALICE).margin;

    set_leverage(&mut ctx, 2).unwrap();
    assert_eq!(position(&mut ctx, ALICE).margin, margin);
    let row = risk_row(&mut ctx);
    assert_regime(&row, Regime::Equity);
    assert_eq!(row.positionInitialMargin, 150_000_000);
    assert_eq!(cap_of(&row), 149_700_000);
    assert_refused_whole(&mut ctx, 149_700_001, G3);
    // The accept side at L = 2, on a fresh open — taking the cap here would move the silo the
    // L = 3 step below reads.
    let (mut fresh, _) = opened(true, Q as u64, 1, TAKER_PPM);
    set_leverage(&mut fresh, 2).unwrap();
    assert_eq!(probe(&mut fresh, G3), 149_700_000);

    set_leverage(&mut ctx, 3).unwrap();
    assert_eq!(position(&mut ctx, ALICE).margin, margin);
    let row = risk_row(&mut ctx);
    assert_regime(&row, Regime::Equity);
    assert_eq!(row.positionInitialMargin, 100_000_000);
    assert_eq!(probe(&mut ctx, G3), 199_700_000);
}

// ── T13: the property test ───────────────────────────────────────────────────────────────

/// Deterministic xorshift (same generator as the settlement conservation fuzz).
fn next(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn tier(lower_bound_notional: u64, max_leverage: u32) -> MarginTier {
    MarginTier {
        lower_bound_notional,
        max_leverage,
        maint_margin_rate_ppm: 0,
    }
}

/// T13: on random states — both signs, `L` 1..=3, random size / entry / margin / mark, random
/// tier tables (including ones with `MM > PIM`), on the fixture grid AND on a fractional
/// `price_decimals = 8` grid where the notional truncates and `PIM` rounds up — the gate's
/// accept boundary is EXACTLY the cap computed from `getPositionRisk`: `cap + 1` is refused
/// whole by the gate the row names, and `cap` is honoured to the unit. About a third of the
/// states also rest orders, so `IM ≠ PIM` there and a G3 reading `initialMargin` fails. Coverage
/// is asserted, not hoped for.
#[test]
fn t13_the_accept_boundary_is_the_get_position_risk_cap_on_random_states() {
    let mut seed: u64 = 0x2b5e_ed00_c0ff_ee11;
    // The resting orders draw from their OWN stream, so the positions, marks and tables are the
    // same 3_000 states with or without them.
    let mut order_seed: u64 = 0x0dd5_1de5_b1d5_a5c3;
    let mut gates: std::collections::BTreeMap<&str, usize> = Default::default();
    let mut regimes: std::collections::BTreeMap<Option<Regime>, usize> = Default::default();
    let (mut clamped, mut taken, mut frac_truncated, mut pim_rounded) = (0, 0, 0, 0);
    let mut with_ooim = 0;
    for _ in 0..3_000 {
        let frac = next(&mut seed) % 2 == 1;
        let (market_id, price_decimals) = if frac {
            (FRAC_MARKET, FRAC_PRICE_DECIMALS)
        } else {
            (MARKET_ID, PRICE_DECIMALS)
        };
        let entry_cents = 1_000 + next(&mut seed) % 99_000;
        let entry = if frac {
            entry_cents * 1_000_000 + next(&mut seed) % 1_000_000
        } else {
            entry_cents
        };
        let qty = 1 + next(&mut seed) % 500;
        let long = next(&mut seed) % 2 == 0;
        let n_entry = calc_value(entry, qty, 0, price_decimals).unwrap() as i64;
        let jitter = (next(&mut seed) % 2_001) as i64 - 1_000;
        let v_quote_abs = (n_entry + jitter).max(1);
        let lev = 1 + next(&mut seed) % 3;
        let mark_pct = 50 + next(&mut seed) % 101;
        let mark = ((entry as u128 * mark_pct as u128 / 100) as u64 + next(&mut seed) % 7).max(1);
        // Up to `2N` half the time: a `MM > PIM` table needs equity above `N/2` before G4 has
        // anything left to bind on.
        let margin = match next(&mut seed) % 16 {
            0 => 0,
            1 => (next(&mut seed) % 1_000) as i64,
            2..=8 => (next(&mut seed) % (2 * n_entry as u64 / lev + 1)) as i64,
            _ => (next(&mut seed) % (2 * n_entry as u64 + 1)) as i64,
        };
        let x = 1 + next(&mut seed) % (2 * n_entry as u64);
        let rows = match next(&mut seed) % 6 {
            0 | 1 => vec![tier(0, 3)],
            2 => vec![tier(0, 3), tier(x, 1)],
            3 => vec![tier(0, 1)],
            4 => vec![tier(0, 2)],
            _ => vec![tier(0, 3), tier(x, 2), tier(2 * x, 1)],
        };
        let wallet = (next(&mut seed) % 1_000_000_000_000) as i64;

        // One resting buy and/or one resting sell, each worth up to ~3× the entry notional:
        // `Bid`/`Ask` move `IM` off `PIM` and must not move the cap. They are installed as real
        // order lists with the side aggregates folded from them, never as bare aggregates:
        // `compute_margin_info`'s debug-only oracle compares the stored `Bid`/`Ask` against
        // that fold on every `getPositionRisk` read AND on the account fold behind every
        // accepted withdrawal's `AccountBalanceChanged`, so aggregates with no orders behind
        // them would trip it.
        let mut sides: [std::collections::VecDeque<crate::types::OrderEntry>; 2] =
            Default::default();
        if next(&mut order_seed) % 3 == 0 {
            for (i, list) in sides.iter_mut().enumerate() {
                if next(&mut order_seed) % 4 == 0 {
                    continue;
                }
                let price = ((entry as u128 * (50 + next(&mut order_seed) % 101) as u128)
                    / 100)
                    .max(1) as u64;
                let mut order_id = [0u8; 32];
                order_id[31] = 1 + i as u8;
                list.push_back(crate::types::OrderEntry {
                    order_id,
                    price,
                    amount: 1 + next(&mut order_seed) % (2 * qty),
                    maker_fee_ppm: 0,
                    assuming_price: price,
                    reduce_only: false,
                    stp: Default::default(),
                });
            }
        }
        let [buys, sells] = sides;
        let fold = |list: &std::collections::VecDeque<crate::types::OrderEntry>| {
            crate::math::sum_side_totals(list.iter().copied(), 0, price_decimals).unwrap()
        };
        let ((buy_qty, bid), (sell_qty, ask)) = (fold(&buys), fold(&sells));

        let mut ctx = make_ctx();
        storage::save_admin(&mut ctx, ADMIN).unwrap();
        storage::save_market(
            &mut ctx,
            &Market {
                market_id,
                base_decimals: 0,
                price_decimals,
                tick_size: 1,
                step_size: 1,
                min_quantity: 1,
                max_quantity: 1_000_000,
                max_price: u64::MAX / 4,
                price_update_interval: 15,
                active: true,
                funding_interval: 0,
                interest_rate: 0,
                liquidation_fee_rate_bps: 0,
                price_band_bps: 0,
                mark_price: mark,
                tiers: MarginTiers::from_tiers(&rows).unwrap(),
                basis_mode: 0,
                deferred_work: 0,
                cumulative_funding_index: 0,
            },
        )
        .unwrap();
        storage::save_account(
            &mut ctx,
            ALICE,
            UserAccount {
                perp_wallet_balance: wallet,
                ..UserAccount::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
        storage::save_position(
            &mut ctx,
            ALICE,
            market_id,
            &PerpPosition {
                amount: if long { qty as i64 } else { -(qty as i64) },
                v_quote_balance: if long { -v_quote_abs } else { v_quote_abs },
                margin,
                leverage: lev,
                total_buy_qty: buy_qty,
                total_buy_notional: bid,
                total_sell_qty: sell_qty,
                total_sell_notional: ask,
                ..crate::margin::margin_type::never_set_position()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
        if !buys.is_empty() {
            storage::save_buy_orders(&mut ctx, ALICE, market_id, &buys).unwrap();
        }
        if !sells.is_empty() {
            storage::save_sell_orders(&mut ctx, ALICE, market_id, &sells).unwrap();
        }

        let row = risk_row_on(&mut ctx, ALICE, market_id);
        assert_eq!((row.bidNotional, row.askNotional), (bid, ask));
        if row.openOrderInitialMargin > 0 {
            with_ooim += 1;
        }
        let cap = cap_of(&row);
        let gate = gate_for(&row, cap + 1).expect("cap + 1 must fail some gate");
        *gates.entry(gate).or_default() += 1;
        *regimes.entry(regime_of(&row)).or_default() += 1;
        if frac && (mark as u128 * qty as u128) % 100 != 0 {
            frac_truncated += 1;
        }
        if row.notional % lev != 0 {
            pim_rounded += 1;
        }
        assert_eq!(probe_on(&mut ctx, ALICE, market_id, gate), cap);
        if cap == 0 {
            clamped += 1;
        } else {
            taken += 1;
        }
    }
    for g in [G1, G2, G3, G4] {
        assert!(gates.get(g).copied().unwrap_or(0) >= 20, "gate `{g}` under-covered: {gates:?}");
    }
    for r in [Regime::Equity, Regime::Balance, Regime::SafetyNet] {
        assert!(regimes.get(&Some(r)).copied().unwrap_or(0) >= 20, "{r:?}: {regimes:?}");
    }
    assert!(clamped >= 100 && taken >= 1_000, "clamped {clamped}, taken {taken}");
    assert!(frac_truncated >= 500, "truncating notionals: {frac_truncated}");
    assert!(pim_rounded >= 500, "rounded-up PIMs: {pim_rounded}");
    assert!(with_ooim >= 200, "states with openOrderInitialMargin > 0: {with_ooim}");
}

// ── T15: the G4 safety net ───────────────────────────────────────────────────────────────

/// T15 (+ T9), route ①: `setMarginTiers` installs a LOWER-leverage table under a live losing
/// `L = 3` long — nothing re-checks open positions. Long 10, `vQuote` −1.05e9 (entry $105),
/// margin 600M, mark $100, tiers `[0, 100M] / [3×, 1×]`: `MM = 100M/6 + 900M/2 = 466_666_666`
/// against `PIM = 333_333_334`, `uPnL = −50M`. Terms G2 133_333_334, G3 216_666_666, G4
/// 83_333_334 — G4 binds, and `cap + 1` clears G2 and G3 and fails G4 with its FULL string.
/// Equity then sits exactly on `MM`, and `liquidate()` is refused.
#[test]
fn t15_g4_binds_after_set_margin_tiers_lowers_leverage_under_a_live_position() {
    let mut ctx = seeded(QTY, -1_050_000_000, 600_000_000, 3, ENTRY_PRICE);
    let row = risk_row(&mut ctx);
    assert_regime(&row, Regime::Equity);
    assert_eq!(cap_of(&row), 216_666_666, "on the default table G3 binds");

    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 100_000_000], &[3, 1]).unwrap();
    let row = risk_row(&mut ctx);
    assert_regime(&row, Regime::SafetyNet);
    assert_eq!(row.maintMargin, 466_666_666);
    assert_eq!(row.positionInitialMargin, 333_333_334);
    assert_eq!(row.unrealizedProfit, -50_000_000);
    assert_eq!(probe(&mut ctx, G4), 83_333_334);
    let after = risk_row(&mut ctx);
    assert_eq!(after.isolatedMargin, after.maintMargin as i64, "equity == MM exactly");
    assert_not_liquidatable(&mut ctx);
}

/// T15 (+ T9), route ②: a SHORT whose notional drifts into a 1× tier as the mark rises. Tiers
/// `[0, 150M] / [3×, 1×]` are in place BEFORE a real-fill `L = 3` open of 1 @ $100 (`N = 1e8`,
/// tier 0, so the per-open tier guard passes), then `addPositionMargin(500M)`. At $350
/// `|N| = 3.5e8 > 2·150M`: `MM = 150M/6 + 200M/2 = 125M > PIM = ⌈3.5e8/3⌉ = 116_666_667`,
/// `uPnL = −250M`. The cap `iw + uPnL − MM` is G4's.
#[test]
fn t15_g4_binds_when_a_short_drifts_into_a_lower_leverage_tier() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 150_000_000], &[3, 1]).unwrap();
    let fee = open_via_fill(&mut ctx, false, 1, 3, TAKER_PPM);
    assert_eq!(fee, 100_000);
    add_position_margin(&mut ctx, 500_000_000).unwrap();
    let iw = 100_000_000 / 3 - fee as i64 + 500_000_000;
    assert_eq!(position(&mut ctx, ALICE).margin, iw);
    set_mark(&mut ctx, 35_000);
    let row = risk_row(&mut ctx);
    assert_regime(&row, Regime::SafetyNet);
    assert_eq!(row.maintMargin, 125_000_000);
    assert_eq!(row.positionInitialMargin, 116_666_667);
    assert_eq!(row.unrealizedProfit, -250_000_000);
    let want = (iw - 250_000_000 - 125_000_000) as u64;
    assert_eq!(want, 158_233_333);
    assert_eq!(probe(&mut ctx, G4), want);
    assert_not_liquidatable(&mut ctx);
}

// ── T16: life after the cap ──────────────────────────────────────────────────────────────

fn total_wallet(ctx: &mut TestCtx) -> i64 {
    storage::load_account(ctx, ALICE).unwrap().perp_wallet_balance + position(ctx, ALICE).margin
}

/// Every `PositionChanged` row in `logs` has a NON-NEGATIVE margin. **Only valid for a scenario
/// with no funding DEBT.** A funding charge lands on `pos.margin` whole and may drive it below
/// zero (`crate::risk::funding`), and the rows then report that debt — correctly. The T16
/// scenarios
/// that use this charge at most 60_000 against a 160e6 silo, so here a negative row can only
/// mean a broken close release; a scenario that charges more funding than the silo holds must
/// not use this helper (see `mod funding_debt` for those).
fn assert_margins_nonnegative_absent_funding_debt(logs: &[primitives::Log]) {
    for row in decode::<PositionChanged>(logs) {
        assert!(row.margin >= 0, "PositionChanged({}) margin {}", row.user, row.margin);
    }
}

/// `getPositionRisk.liquidationPrice` is the flip point of the production predicate: for a
/// long liquidatable AT it and not one tick above; for a short at it and not one tick below.
fn assert_liquidation_price_is_the_predicates_flip(ctx: &mut TestCtx) -> u64 {
    let lp = risk_row(ctx).liquidationPrice;
    let pos = position(ctx, ALICE);
    let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    let above = |p: u64| {
        is_above_maintenance_margin(
            &market.tiers,
            p,
            pos.amount,
            pos.v_quote_balance,
            pos.margin,
            0,
            PRICE_DECIMALS,
        )
        .unwrap()
    };
    assert!(lp > 0);
    assert!(!above(lp), "liquidatable at the reported price {lp}");
    if pos.amount > 0 {
        assert!(above(lp + 1), "and not one tick above it");
    } else {
        assert!(above(lp - 1), "and not one tick below it");
    }
    lp
}

/// Long 6 @ $100 at `L = 3` through a real fill (10 bps: fee 600_000, silo 199.4e6), marked to
/// $110 (`PIM` 2.2e8, `MM` 1.1e8, `uPnL` 6e7), and the cap 39.4e6 taken: the silo is 160e6 —
/// BELOW `PIM`. Its liquidation price is $87.99: `6e4·P − 4.4e8 ≥ 1e4·P ⇔ P ≥ 8_800`.
fn withdrawn_to_cap() -> TestCtx {
    let (mut ctx, fee) = opened(true, 6, 3, TAKER_PPM);
    assert_eq!(fee, 600_000);
    set_mark(&mut ctx, 11_000);
    assert_regime(&risk_row(&mut ctx), Regime::Equity);
    assert_eq!(probe(&mut ctx, G3), 39_400_000);
    let row = risk_row(&mut ctx);
    assert_eq!(row.isolatedWallet, 160_000_000);
    assert!(row.isolatedWallet < row.positionInitialMargin as i64);
    assert_eq!(assert_liquidation_price_is_the_predicates_flip(&mut ctx), 8_799);
    ctx
}

/// ALICE sells `qty` at the $110 mark into a MAKER bid. Returns `(takerFee, takerRealizedPnl,
/// ALICE's settled funding, logs)`.
fn alice_sells_at_110(ctx: &mut TestCtx, qty: u64) -> (u64, i64, i64, Vec<primitives::Log>) {
    place_maker_order(ctx, Side::Buy as u8, 11_000, qty);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    try_place_order(ctx, ALICE, Side::Sell as u8, 11_000, qty).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());
    let trades = decode::<Trade>(&logs);
    assert_eq!(trades.len(), 1);
    assert_eq!((trades[0].taker, trades[0].quantity), (ALICE, qty));
    let funding = decode::<FundingSettled>(&logs)
        .into_iter()
        .filter(|f| f.user == ALICE)
        .map(|f| f.amount)
        .sum();
    assert_margins_nonnegative_absent_funding_debt(&logs);
    (trades[0].takerFee, trades[0].takerRealizedPnl, funding, logs)
}

/// T16 ①: a 50% close after the cap, with a funding charge pending (settled by the fill):
/// total wallet moves by `realized − fee + funding` = 3e7 − 330_000 − 60_000; the half left
/// keeps a non-negative silo and an exact liquidation price.
#[test]
fn t16_partial_close_after_the_cap() {
    let mut ctx = withdrawn_to_cap();
    set_funding_index(&mut ctx, 1_000_000);
    let tw0 = total_wallet(&mut ctx);
    let (fee, realized, funding, _) = alice_sells_at_110(&mut ctx, 3);
    assert_eq!((fee, realized, funding), (330_000, 30_000_000, -60_000));
    assert_eq!(total_wallet(&mut ctx) - tw0, realized - fee as i64 + funding);
    let pos = position(&mut ctx, ALICE);
    assert_eq!(pos.amount, 3);
    assert_eq!(pos.margin, (160_000_000 - 60_000) / 2, "half the settled silo stays");
    assert_liquidation_price_is_the_predicates_flip(&mut ctx);
}

/// T16 ②: a flip after the cap (sell 12): the closing leg realizes 6e7, the opening short is
/// funded `⌊6.6e8/3⌋` from the wallet with the whole 1.32e6 fee out of it; total wallet moves by
/// `realized − fee`.
#[test]
fn t16_flip_after_the_cap() {
    let mut ctx = withdrawn_to_cap();
    let tw0 = total_wallet(&mut ctx);
    let (fee, realized, funding, _) = alice_sells_at_110(&mut ctx, 12);
    assert_eq!((fee, realized, funding), (1_320_000, 60_000_000, 0));
    assert_eq!(total_wallet(&mut ctx) - tw0, realized - fee as i64);
    let pos = position(&mut ctx, ALICE);
    assert_eq!(
        (pos.amount, pos.v_quote_balance, pos.margin),
        (-6, 660_000_000, 220_000_000 - 1_320_000)
    );
    assert_liquidation_price_is_the_predicates_flip(&mut ctx);
}

/// T16 ③: a full close after the cap: realized 6e7, fee 660_000, silo released to 0.
#[test]
fn t16_full_close_after_the_cap() {
    let mut ctx = withdrawn_to_cap();
    let tw0 = total_wallet(&mut ctx);
    let (fee, realized, funding, _) = alice_sells_at_110(&mut ctx, 6);
    assert_eq!((fee, realized, funding), (660_000, 60_000_000, 0));
    assert_eq!(total_wallet(&mut ctx) - tw0, realized - fee as i64);
    let pos = position(&mut ctx, ALICE);
    assert_eq!((pos.amount, pos.margin), (0, 0));
    assert_eq!(risk_row(&mut ctx).liquidationPrice, 0, "flat: no liquidation price");
}

/// T16 ④: liquidation after the cap. The reported $87.99 is honoured by `liquidate()` on both
/// sides — refused at $88.00, executed at $87.99 into a MAKER bid. Taker fee waived; a 5%
/// clearance fee on the $527.94 notional closed (26.397e6 — under its J5 cap, the 87.94e6 the
/// close returned to the wallet) goes to the insurance fund. Realized
/// `6·87.99·1e6 − 6e8 = −72_060_000`; the silo covers it (no bad debt); total wallet moves by
/// `realized − clearance fee`.
#[test]
fn t16_liquidation_after_the_cap() {
    let mut ctx = withdrawn_to_cap();
    let mut market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    market.liquidation_fee_rate_bps = 500;
    storage::save_market(&mut ctx, &market).unwrap();

    set_mark(&mut ctx, 8_800);
    assert_not_liquidatable(&mut ctx);
    set_mark(&mut ctx, 8_799);
    assert_eq!(assert_liquidation_price_is_the_predicates_flip(&mut ctx), 8_799);
    place_maker_order(&mut ctx, Side::Buy as u8, 8_799, 6);
    let tw0 = total_wallet(&mut ctx);
    let if0 = storage::load_insurance_fund(&mut ctx).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());
    liquidate(&mut ctx, ALICE).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_margins_nonnegative_absent_funding_debt(&logs);

    let trades = decode::<Trade>(&logs);
    assert_eq!(trades.len(), 1);
    assert_eq!(
        (trades[0].taker, trades[0].takerFee, trades[0].takerRealizedPnl),
        (ALICE, 0, -72_060_000)
    );
    let liquidation = decode::<Liquidation>(&logs);
    assert_eq!(liquidation.len(), 1);
    let clearance = liquidation[0].reward;
    assert_eq!(clearance, 26_397_000);
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap() - if0, clearance);
    assert_eq!(
        total_wallet(&mut ctx) - tw0,
        trades[0].takerRealizedPnl - clearance as i64
    );
    let pos = position(&mut ctx, ALICE);
    assert_eq!((pos.amount, pos.margin), (0, 0));
}
