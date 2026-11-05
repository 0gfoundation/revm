use super::*;
use crate::interface::IPerpDex::placeAlgoOrderCall;
use crate::types::AlgoOrderType;

const RO: u8 = 1;
const CP: u8 = 2;

fn stop_market(ctx: &mut TestCtx, user: Address, trigger: u64, qty: u64) -> [u8; 32] {
    let out = crate::trading::algo::run_place_algo_order(
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: Side::Sell as u8,
            orderType: AlgoOrderType::StopMarket as u8,
            triggerPrice: trigger,
            price: 0,
            quantity: qty,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: RO,
        }
        .abi_encode(),
        user,
        ctx,
    )
    .unwrap();
    out[..32].try_into().unwrap()
}

/// A stop-loss fires on the mark and closes the position against the book.
#[test]
fn a_stop_market_fires_and_closes_against_the_book() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    // Liquidity to close into, inside the fill-time band around the $95 mark.
    place_order(&mut ctx, MAKER, Side::Buy as u8, 9_400, QTY as u64);
    let id = stop_market(&mut ctx, ALICE, 9_500, QTY as u64);

    // $100 -> $95: at the trigger, and ALICE is still comfortably above maintenance, so this
    // is the stop firing rather than a liquidation.
    oracle_tick(&mut ctx, 9_500, 31);

    assert_eq!(position(&mut ctx, ALICE).amount, 0, "the stop closed the position");
    assert!(
        storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty(),
        "and left the conditional namespace"
    );
    assert!(storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
    assert!(
        storage::load_order(&mut ctx, &id).unwrap().is_none(),
        "fully filled market order: delete-on-terminal, same as any other"
    );
}

/// Above the trigger nothing happens — the sweep must not fire early.
#[test]
fn it_does_not_fire_before_the_mark_reaches_the_trigger() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    let id = stop_market(&mut ctx, ALICE, 9_000, QTY as u64);

    // ⚠️ $99, not $95. At $95 this 5x long is already BELOW maintenance (equity $150 against a
    // ~$158 requirement at mmr = 1/6) and the liquidation sweep closes it — which would make
    // this test pass for entirely the wrong reason the day the trigger logic broke.
    oracle_tick(&mut ctx, 9_900, 31);

    assert_eq!(position(&mut ctx, ALICE).amount, QTY, "mark 9900 > trigger 9000");
    assert!(storage::load_order(&mut ctx, &id).unwrap().is_some());
    assert_eq!(storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap(), vec![9_000]);
}

/// ⚠️ THE DIVERGENCE FROM BINANCE, and the reason it is safe.
///
/// Binance LATCHES: once the trigger price is touched the order becomes a normal order and a
/// retrace cannot undo that. We do not latch — the sweep re-derives from the CURRENT mark
/// every run — so a trigger the budget could not reach does not fire later at a price that no
/// longer exists. Same rule the fill-time band imposes: something declined to execute it, so
/// it did not execute.
///
/// Here the position is closed by other means between the two ticks, which is the cleanest way
/// to observe "the condition is re-checked rather than remembered": the order fires, finds
/// nothing to close, and expires instead of opening a short.
#[test]
fn a_trigger_whose_position_vanished_expires_instead_of_opening() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    let id = stop_market(&mut ctx, ALICE, 9_500, QTY as u64);

    // Close the position out from under the conditional order, by hand.
    let mut pos = storage::load_position(&mut ctx, ALICE, MARKET_ID).unwrap();
    pos.amount = 0;
    pos.v_quote_balance = 0;
    pos.margin = 0;
    storage::save_position(&mut ctx, ALICE, MARKET_ID, &pos, AccountUpdateReason::Adjustment)
        .unwrap();

    oracle_tick(&mut ctx, 9_500, 31);

    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "a reduce-only stop must NEVER open exposure — this is the whole point of the flag"
    );
    assert!(storage::load_order(&mut ctx, &id).unwrap().is_none(), "expired");
    assert!(storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
}

#[allow(clippy::too_many_arguments)]
fn place_algo(
    ctx: &mut TestCtx,
    user: Address,
    side: Side,
    kind: AlgoOrderType,
    trigger: u64,
    price: u64,
    qty: u64,
    flags: u8,
) -> [u8; 32] {
    let out = crate::trading::algo::run_place_algo_order(
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: side as u8,
            orderType: kind as u8,
            triggerPrice: trigger,
            price,
            quantity: qty,
            tif: if kind.is_market() { 0 } else { 0 }, // GTC for the limit pair
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags,
        }
        .abi_encode(),
        user,
        ctx,
    )
    .unwrap();
    out[..32].try_into().unwrap()
}

/// **The LIMIT variant's trigger path, which had no test.** Every trigger test used
/// `STOP_MARKET`; `AlgoOrderType::Stop` appeared only in two REJECTION assertions, so the
/// branch that hands a fired order to `place_order_core` as a resting LIMIT had never run.
///
/// This is also the half of the Binance parity pair that does NOT expire: a market stop says
/// "now or not at all", a limit stop says "at this price or better, and keep waiting".
#[test]
fn a_limit_stop_rests_in_the_book_when_it_fires() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    // No liquidity on purpose: the fired order must REST, which is the thing being pinned.
    let id = place_algo(&mut ctx, ALICE, Side::Sell, AlgoOrderType::Stop, 9_900, 9_950, QTY as u64, RO);

    // ⚠️ 9_900, not 9_500. At 9_500 this 5x long is already BELOW maintenance (equity $150
    // against a ~$158 requirement), so the liquidation sweep closes it and cancels every order
    // she holds — including the one just rested. The test would then be measuring liquidation.
    oracle_tick(&mut ctx, 9_900, 31);

    assert_eq!(
        storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![9_950],
        "the fired limit order must be RESTING in the book at its own price"
    );
    let o = storage::load_order(&mut ctx, &id).unwrap().expect("still live");
    assert_eq!(o.status, OrderStatus::Open);
    assert_eq!(o.trigger_price, 0, "it is an ordinary order now, not a conditional one");
    assert_eq!(
        position(&mut ctx, ALICE).amount,
        QTY,
        "nothing matched it, so the position is untouched — it is waiting, not failed"
    );
    assert!(
        storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty(),
        "and it left the conditional namespace on the way"
    );
}

/// A conditional order's STP mode is set at `placeAlgoOrder` and INHERITED by the ordinary
/// order its trigger produces — through the one flags encoder, so the fired order's
/// `OrderPlaced`, `OrderRested` and `getOrder` all echo reduce-only plus that mode, and
/// `getAlgoOrder` reports it as a value while the order waits.
#[test]
fn a_fired_conditional_inherits_its_stp_mode() {
    use crate::interface::IPerpDex::{getAlgoOrderCall, OrderPlaced, OrderRested};
    use crate::types::StpMode;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    let flags = RO | StpMode::ExpireBoth.to_flags();
    let id = place_algo(&mut ctx, ALICE, Side::Sell, AlgoOrderType::Stop, 9_900, 9_950, QTY as u64, flags);

    let placed = JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .find(|l| {
            l.data.topics().first()
                == Some(&crate::interface::IPerpDex::AlgoOrderPlaced::SIGNATURE_HASH)
        })
        .map(|l| {
            crate::interface::IPerpDex::AlgoOrderPlaced::decode_raw_log(
                l.data.topics(),
                &l.data.data,
            )
            .unwrap()
            .flags
        });
    assert_eq!(placed, Some(flags), "AlgoOrderPlaced echoes the raw (= canonical) byte");
    let ret = crate::trading::algo::run_get_algo_order(
        &getAlgoOrderCall { algoId: id.into() }.abi_encode(),
        &mut ctx,
    )
    .unwrap();
    let view = getAlgoOrderCall::abi_decode_returns(&ret).unwrap();
    assert_eq!(view.selfTradePreventionMode, StpMode::ExpireBoth as u8);
    assert!(view.reduceOnly);

    oracle_tick(&mut ctx, 9_900, 31);

    let logs = JournalTr::take_logs(ctx.journal_mut());
    let echoed: Vec<u8> = logs
        .iter()
        .filter_map(|l| match l.data.topics().first() {
            Some(t) if *t == OrderPlaced::SIGNATURE_HASH => Some(
                OrderPlaced::decode_raw_log(l.data.topics(), &l.data.data).unwrap().flags,
            ),
            Some(t) if *t == OrderRested::SIGNATURE_HASH => Some(
                OrderRested::decode_raw_log(l.data.topics(), &l.data.data).unwrap().flags,
            ),
            _ => None,
        })
        .collect();
    assert_eq!(echoed, vec![flags, flags], "OrderPlaced + OrderRested of the fired order");
    let o = storage::load_order(&mut ctx, &id).unwrap().expect("rested");
    assert_eq!(o.trigger_price, 0, "an ordinary order now");
    assert_eq!(o.stp, StpMode::ExpireBoth, "inherited, not reset to the default");
    assert!(o.reduce_only);
}

/// A trigger is never refused by the per-market open-order cap, even when its owner sits AT
/// the cap: `fire` takes the conditional off the owner's list before placing it, so the fired
/// limit order converts one counted open order into another. Were it refused, the refusal
/// would land inside the ORACLE transaction.
///
/// ⚠️ Mutation note: "check the cap on the trigger path too" is an EQUIVALENT mutation — by the
/// argument above the count a trigger sees is at most 199, so the check would pass. What this
/// pins is the outcome (rests, no error, still exactly at the cap), and the `Triggered` arm's
/// `debug_assert!` pins the invariant that makes the skip safe.
#[test]
fn a_trigger_at_the_open_order_cap_still_rests() {
    use crate::trading::algo::MAX_OPEN_ORDERS_PER_MARKET;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, 100_000_000_000);
    let id = place_algo(&mut ctx, ALICE, Side::Sell, AlgoOrderType::Stop, 9_900, 9_950, QTY as u64, RO);
    // 199 resting bids, inside the fill-time band so the oracle tick's band expiry leaves them
    // alone, and below the stop's price so the fired sell does not cross them.
    for _ in 0..MAX_OPEN_ORDERS_PER_MARKET - 1 {
        place_order(&mut ctx, ALICE, Side::Buy as u8, 9_000, 1);
    }
    let open = |ctx: &mut TestCtx| {
        storage::book_order_count(ctx, ALICE, MARKET_ID).unwrap()
            + storage::load_user_algo_orders(ctx, ALICE, MARKET_ID).unwrap().len()
    };
    assert_eq!(open(&mut ctx), MAX_OPEN_ORDERS_PER_MARKET, "fixture: exactly at the cap");
    let e = try_place_order(&mut ctx, ALICE, Side::Buy as u8, 9_000, 1).unwrap_err();
    assert!(format!("{e:?}").contains("max open orders reached"), "got {e:?}");

    oracle_tick(&mut ctx, 9_900, 31);

    assert_eq!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(), vec![9_950]);
    assert_eq!(
        storage::load_order(&mut ctx, &id).unwrap().expect("rested").status,
        OrderStatus::Open
    );
    assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
    assert_eq!(open(&mut ctx), MAX_OPEN_ORDERS_PER_MARKET, "one counted order became another");
}

/// **`closePosition` reads the position AT TRIGGER TIME, which had no test.** The flag only
/// appeared in rejection assertions, so the branch that substitutes `|position|` for the
/// (necessarily zero) `quantity` had never run.
///
/// The fixture grows the position between arming and firing, which is what makes it
/// discriminating twice over: a bug using the stored `quantity` would close 0 and EXPIRE the
/// order, and a bug snapshotting the size at PLACEMENT would close only half.
#[test]
fn close_position_takes_the_size_the_position_has_when_it_fires() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(
        &mut ctx, ALICE, QTY / 2, -ENTRY_VALUE / 2, MARGIN / 2, 5, USER_WALLET as i64,
    );
    // quantity = 0 — mandatory with closePosition, and exactly what a naive implementation
    // would then try to close. CP alone: reduceOnly cannot be sent with it.
    place_algo(&mut ctx, ALICE, Side::Sell, AlgoOrderType::StopMarket, 9_500, 0, 0, CP);

    // Grow it AFTER arming.
    seed_position_account(
        &mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64,
    );
    place_order(&mut ctx, MAKER, Side::Buy as u8, 9_400, QTY as u64);

    oracle_tick(&mut ctx, 9_500, 31);

    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "the WHOLE position at trigger time, not the half it was armed against and not zero"
    );
}

/// **The band-blocked remainder EXPIRES, which I asserted in a commit message and never
/// tested.** The claim was that routing a fired `_MARKET` order through the ordinary path
/// gives Binance's "unfilled market order expires" for free, with no special case. Free or
/// not, it was unverified.
///
/// Liquidity exists here — it is simply OUTSIDE the fill-time band around the post-drop mark,
/// which is precisely the fast-move shape where a stop is most wanted and least fillable.
#[test]
fn a_market_stop_the_band_blocks_expires_rather_than_resting() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    // The band is +/-10% of the mark, so at a 9_500 mark it is [8_550, 10_450]. A bid at 7_000
    // is real liquidity the band will not let the close reach.
    place_order(&mut ctx, MAKER, Side::Buy as u8, 7_000, QTY as u64);
    let id = place_algo(&mut ctx, ALICE, Side::Sell, AlgoOrderType::StopMarket, 9_500, 0, QTY as u64, RO);

    oracle_tick(&mut ctx, 9_500, 31);

    assert_eq!(
        position(&mut ctx, ALICE).amount,
        QTY,
        "nothing in band to close into, so nothing closed"
    );
    assert!(
        storage::load_order(&mut ctx, &id).unwrap().is_none(),
        "EXPIRED, not resting: a market order means now or not at all (Binance parity)"
    );
    assert!(
        storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
        "and it must NOT have become a resting sell — that would be a different order type"
    );
    assert!(
        storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty(),
        "the conditional is spent either way; the owner must re-arm"
    );
}

/// **The budget-exhaustion path, which had no test at all.** `run_trigger_sweep`'s early
/// return is its ONLY deferral route, and until now nothing exercised it — the cleanup that
/// removed a dead `deferred` local could just as easily have removed the `return Ok(true)`
/// and every test would still have passed.
///
/// The fixture is deliberately cheap: these stops have no position to protect, so each one
/// fires, finds zero capacity and EXPIRES. That still costs one unit of budget apiece, which
/// is exactly what is being measured — the budget meters trigger EXECUTIONS, not fills.
///
/// It also pins the hand-off to the drain: hitting the budget must set `deferred_work`, or a
/// keeper has no way to learn there is anything left to finish.
#[test]
fn exhausting_the_sweep_budget_defers_the_rest_and_arms_the_drain() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let n = crate::risk::MAX_SWEEP_ACTIONS_PER_UPDATE + 5;
    for i in 1..=n {
        let user = Address::from([i as u8; 20]);
        stop_market(&mut ctx, user, 9_500, QTY as u64);
    }
    assert_eq!(
        storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![9_500],
        "all of them share one trigger price, so this is one level with n entries"
    );

    oracle_tick(&mut ctx, 9_500, 31);

    let left = storage::load_algo_level(&mut ctx, MARKET_ID, false, 9_500).unwrap().len();
    assert_eq!(
        left as u32,
        n - crate::risk::MAX_SWEEP_ACTIONS_PER_UPDATE,
        "exactly the budget's worth fired; the remainder is still indexed"
    );
    assert_eq!(
        storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap().deferred_work,
        1,
        "a budget stopped us, not the market — the drain must be told"
    );

    // The drain gets a fresh budget and finishes them, in the SAME block and therefore at the
    // SAME mark that fired them — which is what keeps not-latching safe.
    crate::risk::run_drain_deferred_work(
        &drainDeferredWorkCall { marketId: MARKET_ID }.abi_encode(),
        &mut ctx,
    )
    .unwrap();
    assert!(
        storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
        "the drain cleared the backlog"
    );
}

/// Triggers are served BEFORE liquidations out of the shared budget. Not politeness: a stop
/// that executes removes the position the liquidation sweep would otherwise have had to
/// handle, so spending on triggers reduces what is left to spend on liquidations.
#[test]
fn a_stop_fires_rather_than_the_owner_being_liquidated() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    // In-band liquidity at the $85 mark.
    place_order(&mut ctx, MAKER, Side::Buy as u8, 8_400, QTY as u64);
    stop_market(&mut ctx, ALICE, 8_500, QTY as u64);

    // $85 is below maintenance for this 5x long, so WITHOUT the stop the sweep would
    // liquidate her. The stop runs first and closes it as an ordinary trade instead.
    oracle_tick(&mut ctx, 8_500, 31);

    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    let liq = JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|l| {
            l.data.topics().first()
                == Some(&crate::interface::IPerpDex::Liquidation::SIGNATURE_HASH)
        })
        .count();
    assert_eq!(liq, 0, "closed by her own stop, not liquidated");
}
