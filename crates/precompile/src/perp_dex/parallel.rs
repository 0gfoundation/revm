//! Parallel place/cancel execution for the PerpDEX precompile (catalog #21 spike, step 3c).
//!
//! This module is the perp-specific brain of the parallel matching driver: given a decoded order
//! and the current BBO, it decides how the gated phase must treat the operation. The thread pool,
//! per-slot context, and the AccountGate/BboTicketLock wiring live in the driver; this file holds
//! the pure decision so it can be reasoned about and tested without any concurrency.
//!
//! Decisions (encoding the scheme's rules 2–5 + the review's refinements):
//! - **`RejectInBody`** — a PostOnly limit that crosses: the precompile body self-rejects (revert);
//!   handled entirely in 3c, NOT a downgrade.
//! - **`DowngradeToBarrier`** — anything that must run on the serial barrier (step 3d): a matching
//!   limit (GTC) that crosses (it would take liquidity), an IOC/FOK or Market (taker semantics), or
//!   a cancel that EMPTIES the best level (the new BBO must be recomputed by scanning the book,
//!   which requires all lower-txn_id inserts to have landed — only the all-lower-done barrier
//!   guarantees that). 3c only marks these; the serial re-run is 3d.
//! - **`HoldTicket`** — a non-crossing rest that MOVES the BBO (improves the best on its side): the
//!   margin check + insert + best-update must complete while holding the BBO ticket (rule 5).
//! - **`ReleaseTicket`** — a non-crossing rest that does NOT move the BBO (rests below the best), or
//!   a cancel that does not empty the best level: release the BBO ticket after the decision and do
//!   the account/level mutation under the per-level lock, in parallel (rule 4.iii).
//!
//! `best == 0` is the "no order on this side" sentinel (an empty book side).

use crate::perp_dex::types::order::{OrderType, Side, TimeInForce};
use crate::perp_dex::{storage, trading::place_order_core};
use crate::PrecompileError;
use context::journal::perp_sched::{AccountGate, BboTicketLock};
use context::journal::shared_perp::SharedPerpBook;
use context::{ContextTr, JournalTr};
use primitives::Address;
use std::sync::Arc;
use std::thread;

/// How the gated phase must treat one place/cancel under the BBO ticket. See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockPlan {
    /// PostOnly limit that crosses → the body self-rejects (revert). 3c handles it; no barrier.
    RejectInBody,
    /// Must run on the serial barrier (3d): crossing matching-limit / IOC / FOK / Market / a cancel
    /// that empties the best level.
    DowngradeToBarrier,
    /// Non-crossing rest that moves the BBO → execute fully while holding the BBO ticket.
    HoldTicket,
    /// Non-crossing rest below the best, or a cancel that doesn't empty the best level → release the
    /// ticket and mutate under the per-level lock (parallel).
    ReleaseTicket,
}

/// Decide how to handle a place order given the current BBO. Pure; the driver supplies the BBO it
/// read under the ticket. `best_bid`/`best_ask` are the cached bests (0 = that side is empty).
pub fn classify_place(
    side: Side,
    order_type: OrderType,
    tif: TimeInForce,
    price: u64,
    best_bid: u64,
    best_ask: u64,
) -> LockPlan {
    // Only resting limit orders are parallel-eligible; market + taker TIFs take liquidity → barrier.
    if !matches!(order_type, OrderType::Limit) {
        return LockPlan::DowngradeToBarrier;
    }
    if matches!(tif, TimeInForce::Ioc | TimeInForce::Fok) {
        return LockPlan::DowngradeToBarrier;
    }
    // Cross against the opposite best (0 = that side is empty → cannot cross).
    let crosses = match side {
        Side::Buy => best_ask != 0 && price >= best_ask,
        Side::Sell => best_bid != 0 && price <= best_bid,
    };
    if crosses {
        return match tif {
            // PostOnly that would cross is rejected by the body (revert), not matched.
            TimeInForce::PostOnly => LockPlan::RejectInBody,
            // A matching limit (GTC) that crosses takes liquidity → serial barrier.
            _ => LockPlan::DowngradeToBarrier,
        };
    }
    // Non-crossing rest: does it improve (move) the best on its own side?
    let moves_bbo = match side {
        Side::Buy => best_bid == 0 || price > best_bid,
        Side::Sell => best_ask == 0 || price < best_ask,
    };
    if moves_bbo {
        LockPlan::HoldTicket
    } else {
        LockPlan::ReleaseTicket
    }
}

/// Decide how to handle a cancel given the current BBO and whether removing this order empties its
/// price level. `empties_level` is computed by the driver (the level FIFO holds only this order).
pub fn classify_cancel(
    side: Side,
    price: u64,
    best_bid: u64,
    best_ask: u64,
    empties_level: bool,
) -> LockPlan {
    // The BBO moves only if the cancelled order is AT the best on its side AND removing it empties
    // that level (no other orders remain). Then the new best must be found by scanning the book,
    // which is only correct once all lower-txn_id inserts have landed → serial barrier (3d).
    let at_best = match side {
        Side::Buy => price == best_bid,
        Side::Sell => price == best_ask,
    };
    if at_best && empties_level {
        LockPlan::DowngradeToBarrier
    } else {
        LockPlan::ReleaseTicket
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── place: matching limit (GTC) ───────────────────────────────────────────
    #[test]
    fn gtc_buy_below_best_ask_rests() {
        // best_bid=100, best_ask=110; buy at 105 < 110 → non-cross, and 105 > 100 → moves best_bid.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 105, 100, 110),
            LockPlan::HoldTicket
        );
        // buy at 90 < best_bid 100 → non-cross, doesn't move best_bid → release.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 90, 100, 110),
            LockPlan::ReleaseTicket
        );
    }

    #[test]
    fn gtc_buy_at_or_above_best_ask_downgrades() {
        // buy at 110 >= best_ask 110 → crosses → matches → downgrade.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 110, 100, 110),
            LockPlan::DowngradeToBarrier
        );
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 115, 100, 110),
            LockPlan::DowngradeToBarrier
        );
    }

    #[test]
    fn gtc_sell_mirrors_buy() {
        // sell at 105 > best_bid 100 → non-cross; 105 < best_ask 110 → moves best_ask → hold.
        assert_eq!(
            classify_place(Side::Sell, OrderType::Limit, TimeInForce::Gtc, 105, 100, 110),
            LockPlan::HoldTicket
        );
        // sell at 120 > best_ask 110 → non-cross, doesn't move best_ask → release.
        assert_eq!(
            classify_place(Side::Sell, OrderType::Limit, TimeInForce::Gtc, 120, 100, 110),
            LockPlan::ReleaseTicket
        );
        // sell at 100 <= best_bid 100 → crosses → downgrade.
        assert_eq!(
            classify_place(Side::Sell, OrderType::Limit, TimeInForce::Gtc, 100, 100, 110),
            LockPlan::DowngradeToBarrier
        );
    }

    // ── place: PostOnly ────────────────────────────────────────────────────────
    #[test]
    fn postonly_cross_rejects_not_downgrades() {
        // PostOnly buy at 110 >= best_ask 110 → crosses → REJECT (body reverts), not downgrade.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::PostOnly, 110, 100, 110),
            LockPlan::RejectInBody
        );
        // PostOnly buy at 105 < 110 → non-cross, moves best_bid → hold.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::PostOnly, 105, 100, 110),
            LockPlan::HoldTicket
        );
    }

    // ── place: taker TIFs / market → barrier ────────────────────────────────────
    #[test]
    fn ioc_fok_market_downgrade_regardless() {
        for tif in [TimeInForce::Ioc, TimeInForce::Fok] {
            assert_eq!(
                classify_place(Side::Buy, OrderType::Limit, tif, 90, 100, 110),
                LockPlan::DowngradeToBarrier
            );
        }
        assert_eq!(
            classify_place(Side::Buy, OrderType::Market, TimeInForce::Gtc, 0, 100, 110),
            LockPlan::DowngradeToBarrier
        );
    }

    // ── place: empty book side (best == 0) ──────────────────────────────────────
    #[test]
    fn empty_opposite_side_never_crosses_and_first_order_moves_best() {
        // empty ask side (best_ask=0): a buy can't cross; best_bid=0 too → first order moves best.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 105, 0, 0),
            LockPlan::HoldTicket
        );
        // ask side empty (0) but a resting bid exists at 100; buy at 90 < 100 → non-mover.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 90, 100, 0),
            LockPlan::ReleaseTicket
        );
    }

    // ── cancel ──────────────────────────────────────────────────────────────────
    #[test]
    fn cancel_emptying_best_level_downgrades() {
        // buy order at best_bid 100, removing it empties the level → BBO moves → barrier.
        assert_eq!(
            classify_cancel(Side::Buy, 100, 100, 110, true),
            LockPlan::DowngradeToBarrier
        );
    }

    #[test]
    fn cancel_not_moving_best_releases() {
        // at best but level not emptied (others remain) → BBO unchanged → parallel.
        assert_eq!(
            classify_cancel(Side::Buy, 100, 100, 110, false),
            LockPlan::ReleaseTicket
        );
        // below best, even if it empties that (non-best) level → BBO unchanged → parallel.
        assert_eq!(
            classify_cancel(Side::Buy, 90, 100, 110, true),
            LockPlan::ReleaseTicket
        );
        // sell at best_ask, empties → barrier.
        assert_eq!(
            classify_cancel(Side::Sell, 110, 100, 110, true),
            LockPlan::DowngradeToBarrier
        );
    }
}

// ── Driver: single-work-item place lock-body (step 3c part 2) ─────────────────

/// One parallel-eligible place to run in a slot. The ed25519 verify + decode is hoisted upstream
/// (the dominant cost, parallel before the gates); this is the gated mutation. `rank`/`ticket` are
/// assigned by the batch in txn_id order.
#[derive(Debug, Clone)]
pub struct PlaceWork {
    pub maker: Address,
    pub order_id: [u8; 32],
    pub market_id: u64,
    pub side: u8,
    pub price: u64,
    pub qty: u64,
    pub order_type: u8,
    pub tif: u8,
    pub client_order_id: [u8; 16],
    /// This maker's block-order rank (AccountGate, rule 6).
    pub rank: u64,
    /// This op's BBO ticket (txn_id order within the batch; rules 2+3).
    pub ticket: u64,
}

/// Outcome of a slot's place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceOutcome {
    /// Rested (or no-op) and committed to the book.
    Executed,
    /// The body rejected (PostOnly cross, insufficient margin, …); this slot's book writes were
    /// rolled back. Still a processed tx (gas charged upstream).
    Reverted,
    /// Must run on the serial barrier (step 3d): crossing matching-limit / IOC / FOK / Market.
    Downgrade,
}

/// Run `place_order_core` under the maker's account turn, inside a journal checkpoint so a
/// (non-fatal) rejection rolls back this slot's off-trie writes via the write-set (step 3b).
fn gated_execute<CTX: ContextTr>(
    ctx: &mut CTX,
    account_gate: &AccountGate,
    work: &PlaceWork,
) -> Result<PlaceOutcome, PrecompileError> {
    account_gate.run(work.maker, work.rank, || {
        let cp = ctx.journal_mut().checkpoint();
        match place_order_core(
            work.maker,
            work.order_id,
            work.market_id,
            work.side,
            work.price,
            work.qty,
            work.order_type,
            work.tif,
            work.client_order_id,
            ctx,
        ) {
            Ok(()) => {
                ctx.journal_mut().checkpoint_commit();
                Ok(PlaceOutcome::Executed)
            }
            // TODO(3d): distinguish PrecompileError::Fatal (propagate) from a user/validation
            // revert. The parallel place/cancel set never hits Fatal, so for now any error reverts.
            Err(_e) => {
                ctx.journal_mut().checkpoint_revert(cp);
                Ok(PlaceOutcome::Reverted)
            }
        }
    })
}

/// Execute one parallel place (the lock-body). Classify under the BBO ticket; a mover / PostOnly
/// cross executes under the held ticket (the account rank is waited at the place — Q3), a non-mover
/// releases the ticket then executes under the account gate (rule 4.iii), a crossing matcher / taker
/// is marked `Downgrade` for the serial barrier (step 3d does the re-run).
pub fn parallel_place<CTX: ContextTr>(
    ctx: &mut CTX,
    account_gate: &AccountGate,
    bbo: &BboTicketLock,
    work: &PlaceWork,
) -> Result<PlaceOutcome, PrecompileError> {
    enum UnderTicket {
        Done(Result<PlaceOutcome, PrecompileError>),
        Release,
        Downgrade,
    }
    let under = bbo.run(work.ticket, || -> Result<UnderTicket, PrecompileError> {
        let plan = match (
            Side::from_u8(work.side),
            OrderType::from_u8(work.order_type),
            TimeInForce::from_u8(work.tif),
        ) {
            (Some(side), Some(order_type), Some(tif)) => {
                let best_bid = storage::load_best_bid(ctx, work.market_id)?;
                let best_ask = storage::load_best_ask(ctx, work.market_id)?;
                classify_place(side, order_type, tif, work.price, best_bid, best_ask)
            }
            // Unparseable order fields → let the body reject it under the ticket.
            _ => LockPlan::RejectInBody,
        };
        match plan {
            LockPlan::DowngradeToBarrier => Ok(UnderTicket::Downgrade),
            LockPlan::HoldTicket | LockPlan::RejectInBody => {
                Ok(UnderTicket::Done(gated_execute(ctx, account_gate, work)))
            }
            LockPlan::ReleaseTicket => Ok(UnderTicket::Release),
        }
    })?;
    match under {
        UnderTicket::Done(r) => r,
        UnderTicket::Downgrade => Ok(PlaceOutcome::Downgrade),
        // Ticket released; rest under the account gate (the per-level lock is the book's DashMap
        // entry, taken inside the push/mutate helpers).
        UnderTicket::Release => gated_execute(ctx, account_gate, work),
    }
}

/// Run a batch of parallel-eligible places concurrently against ONE shared book, gated by the
/// `AccountGate` (per maker) and `BboTicketLock` (per-market BBO). Each worker builds a FRESH slot
/// context thread-locally via `make_ctx` (so the journal never crosses a thread → no `Send` bound on
/// the journal), then runs [`parallel_place`]. Results are returned in `items` order. `make_ctx`
/// must return a context whose journal already routes to `book` (i.e. `set_perp_shared(book)` done);
/// the book must already hold the block's prior state (the driver/test seeds it).
pub fn run_place_batch<CTX, F>(
    book: &Arc<SharedPerpBook>,
    account_gate: &AccountGate,
    bbo: &BboTicketLock,
    items: &[PlaceWork],
    make_ctx: F,
) -> Vec<Result<PlaceOutcome, PrecompileError>>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX + Sync,
{
    let make_ctx = &make_ctx;
    thread::scope(|s| {
        let handles: Vec<_> = items
            .iter()
            .map(|item| {
                let book = book.clone();
                s.spawn(move || {
                    let mut ctx = make_ctx(book);
                    parallel_place(&mut ctx, account_gate, bbo, item)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// Barrier step (end of a parallel batch / step-3d phase boundary): restore deterministic FIFO
/// time-priority. During the batch, this block's inserts were appended to each level in racy
/// arrival order; here — serially, after all inserts have landed and BEFORE any match reads a level
/// — we re-sort each touched level's THIS-BLOCK entries by `ticket` (= block-local txn_id order),
/// leaving prior-block entries (already consensus-frozen) in place at the front. The `ticket` is a
/// transient sort key; the committed FIFO stays `Vec<order_id>` (no format change). Idempotent:
/// levels already in order are skipped.
pub fn finalize_place_batch_ordering<CTX, F>(
    book: &Arc<SharedPerpBook>,
    items: &[PlaceWork],
    results: &[Result<PlaceOutcome, PrecompileError>],
    make_ctx: F,
) -> Result<(), PrecompileError>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX,
{
    use std::collections::HashMap;
    // (side, market, price) -> { order_id -> ticket } for this block's executed (rested) places.
    let mut levels: HashMap<(u8, u64, u64), HashMap<[u8; 32], u64>> = HashMap::new();
    for (item, res) in items.iter().zip(results) {
        if matches!(res, Ok(PlaceOutcome::Executed)) {
            levels
                .entry((item.side, item.market_id, item.price))
                .or_default()
                .insert(item.order_id, item.ticket);
        }
    }
    if levels.is_empty() {
        return Ok(());
    }
    let mut ctx = make_ctx(book.clone());
    for ((side, market, price), tickets) in levels {
        let fifo = if side == 0 {
            storage::load_bid_level(&mut ctx, market, price)?
        } else {
            storage::load_ask_level(&mut ctx, market, price)?
        };
        // Split: prior-block entries (keep order) ++ this-block entries (re-sort by ticket).
        let mut prior: Vec<[u8; 32]> = Vec::new();
        let mut this_block: Vec<[u8; 32]> = Vec::new();
        for oid in &fifo {
            if tickets.contains_key(oid) {
                this_block.push(*oid);
            } else {
                prior.push(*oid);
            }
        }
        this_block.sort_by_key(|oid| tickets[oid]);
        prior.extend(this_block);
        if prior != fifo {
            if side == 0 {
                storage::save_bid_level(&mut ctx, market, price, &prior)?;
            } else {
                storage::save_ask_level(&mut ctx, market, price, &prior)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod driver_tests {
    use crate::perp_dex::types::Market;
    use super::*;
    use context::journal::shared_perp::SharedPerpBook;
    use context::{BlockEnv, CfgEnv, Context, Journal, TxEnv};
    use database::InMemoryDB;
    use primitives::{address, hardfork::SpecId};
    use std::sync::Arc;

    type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

    /// A taker TIF (IOC) is downgraded to the barrier without touching the book — and without
    /// needing a seeded market/account (classify decides before the body runs).
    #[test]
    fn parallel_place_taker_tif_downgrades_without_book_writes() {
        let book = Arc::new(SharedPerpBook::new());
        let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        ctx.journal_mut().set_perp_shared(book.clone());
        let gate = AccountGate::new();
        let bbo = BboTicketLock::new();

        let work = PlaceWork {
            maker: address!("1111111111111111111111111111111111111111"),
            order_id: [1u8; 32],
            market_id: 1,
            side: 0,            // Buy
            price: 100,
            qty: 1,
            order_type: 0,      // Limit
            tif: 1,             // IOC → taker → downgrade
            client_order_id: [0u8; 16],
            rank: 0,
            ticket: 0,
        };

        let out = parallel_place(&mut ctx, &gate, &bbo, &work).unwrap();
        assert_eq!(out, PlaceOutcome::Downgrade);
        assert!(book.take_delta().is_empty(), "a downgrade must write nothing to the book");
    }

    const TICK: u64 = 1_000_000_000;
    const QTY: u64 = 1_000_000;
    const WALLET: u64 = 1_000_000_000;
    const MID: u64 = 1;

    fn user_addr(i: u64) -> Address {
        let mut b = [0u8; 20];
        b[12..20].copy_from_slice(&i.to_be_bytes());
        Address::from(b)
    }

    fn test_market() -> Market {
        Market {
            market_id: MID,
            base_decimals: 8,
            price_decimals: 9,
            tick_size: TICK,
            step_size: QTY,
            min_quantity: QTY,
            max_quantity: QTY * 1_000,
            max_price: 100_000 * TICK,
            price_update_interval: 15,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
        }
    }

    fn fund<CTX: ContextTr>(ctx: &mut CTX, user: Address, amt: u64) {
        let mut acc = storage::load_account(ctx, user).unwrap();
        acc.credit_perp(amt).unwrap();
        storage::save_account(ctx, user, acc).unwrap();
    }

    fn seed<CTX: ContextTr>(ctx: &mut CTX, users: &[Address]) {
        storage::save_market(ctx, &test_market()).unwrap();
        for &u in users {
            fund(ctx, u, WALLET);
        }
    }

    fn mk_work(maker: Address, order_id: [u8; 32], price: u64, rank: u64, ticket: u64) -> PlaceWork {
        PlaceWork {
            maker,
            order_id,
            market_id: MID,
            side: 0, // Buy
            price,
            qty: QTY,
            order_type: 0, // Limit
            tif: 0,        // GTC
            client_order_id: [0u8; 16],
            rank,
            ticket,
        }
    }

    fn make_slot(bk: Arc<SharedPerpBook>) -> TestCtx {
        let mut c: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        c.journal_mut().set_perp_shared(bk);
        c
    }

    /// THE step-3c gate: a batch of non-crossing places run in parallel + the barrier-sort produces
    /// a book delta byte-identical to the same ops run serially in txn_id order. Crucially includes
    /// TWO non-movers at the SAME non-best price (B@99, C@99): their FIFO append order is racy in the
    /// parallel phase and is corrected to ticket (txn_id) order by `finalize_place_batch_ordering`.
    /// Also: a mover (A@100), same-account ordering (a's two orders A@100 rank 0, C@99 rank 1), and
    /// three makers.
    #[test]
    fn parallel_place_batch_matches_serial_delta() {
        let a = user_addr(1);
        let b = user_addr(2);
        // A@100 (a, mover), B@99 (b), C@99 (a's 2nd) — B and C share level 99.
        let items = vec![
            mk_work(a, [1u8; 32], 100 * TICK, 0, 0),
            mk_work(b, [2u8; 32], 99 * TICK, 0, 1),
            mk_work(a, [3u8; 32], 99 * TICK, 1, 2),
        ];

        // Serial reference: place_order_core in txn_id (ticket) order against a PerpSection ctx.
        let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut serial, &[a, b]);
        for it in &items {
            place_order_core(
                it.maker,
                it.order_id,
                it.market_id,
                it.side,
                it.price,
                it.qty,
                it.order_type,
                it.tif,
                it.client_order_id,
                &mut serial,
            )
            .unwrap();
        }
        let serial_delta = serial.journal_mut().take_perp_delta();

        // Parallel: same ops via the batch runner against one shared book, then the barrier-sort.
        let book = Arc::new(SharedPerpBook::new());
        seed(&mut make_slot(book.clone()), &[a, b]);
        let gate = AccountGate::new();
        let bbo = BboTicketLock::new();
        let results = run_place_batch(&book, &gate, &bbo, &items, make_slot);
        for r in &results {
            assert_eq!(*r.as_ref().unwrap(), PlaceOutcome::Executed);
        }
        finalize_place_batch_ordering(&book, &items, &results, make_slot).unwrap();
        let parallel_delta = book.take_delta();

        assert_eq!(serial_delta, parallel_delta);
    }
}
