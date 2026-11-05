//! The algo / conditional-order suites. Each one's own `use super::*` reaches the
//! `risk::tests` fixture through the glob below.
use super::*;

/// `placeAlgoOrder` admission. Every rejection here is a rejection Binance would also make, or a
/// deliberate, documented divergence — there are no "we did not get round to it" errors.
mod algo_order_admission;

/// The trigger sweep: when a conditional order fires, what it becomes, and — the part that is a
/// deliberate divergence from Binance — what happens when it does NOT get to fire.
mod algo_trigger_sweep;

/// A fired conditional order is sized by the SAME capacity rule as `placeOrder`'s reduce-only
/// admission (`reduce_only::closing_capacity`): an order that fills immediately is bounded by
/// `|position|` alone, and the resting reduce-only orders it leaves over-committed are trimmed
/// AFTER the fill by the ordinary eviction pass (Binance `er=4`).
///
/// The bug this pins: `fire` used to subtract the owner's resting reduce-only orders (and the
/// normal orders "ahead of the mark") from the capacity of a MARKET fire too. The standard bracket —
/// a reduce-only take-profit LIMIT resting in the book plus a stop-loss conditional — therefore
/// broke whenever the stop fired: with the take-profit at full size the stop closed NOTHING (it
/// expired as `AlgoOrderCancelled(6)`), and with it at half size the stop closed only half. A stop
/// that does not stop is the worst failure this family has.
mod algo_fire_capacity;

/// closePosition parity with Binance's Algo Order API: the flag pair it refuses, the
/// "would immediately trigger" refusal every conditional order gets, and the position binding
/// (`GTE_GTC`) — a closePosition order belongs to the position open when it was placed, and is
/// spent with `PositionClosed` (11) once that position has gone flat or flipped.
mod close_position_parity;

/// The signed (API-key) path for conditional orders — the one real traders use, since the direct
/// call needs an EOA with gas and a nonce. Binance's analogue is `X-MBX-APIKEY` plus an HMAC over
/// the query string; here `keyId` names the registered ed25519 slot and the signature covers a
/// fixed-layout canonical message.
mod algo_order_signed;

/// A triggered conditional order the MATCHER refuses (plan D11). Before the fix the reject propagated
/// out of `fire`, after it had already written (index removal, `AlgoOrderTriggered`), so the oracle
/// transaction reverted over retained perp writes, the rest of the tick's sweep — liquidations
/// included — never ran, and the record was orphaned. Now the refusal is the trigger's terminal
/// outcome: `AlgoOrderCancelled(TriggerRejected = 10)` right after `AlgoOrderTriggered`, the record
/// deleted, and the tick carries on.
///
/// Every tick here goes through the full call shell, whose commit-only tripwire `debug_assert!`s on
/// a revert that left perp writes behind — so the old behaviour fails these tests loudly even
/// before the explicit `!reverted` check.
mod algo_trigger_rejected;

/// Self-trade prevention on a FIRED conditional order (plan T15, and T25's mode variants). The mode
/// is set at `placeAlgoOrder` and INHERITED by the ordinary order the trigger places, which then
/// walks the book like any taker — inside the ORACLE transaction. Every tick goes through the
/// call shell, so a reject that leaked a write would trip its write-then-revert tripwire.
///
/// ALICE (a healthy 5x long the stop protects) also rests her own BID, which a fired sell meets.
/// No liquidation candidate here: the subject is the trigger's own outcome.
mod algo_stp;

/// The algo-order trigger index. Its SORT DIRECTIONS are load-bearing, not cosmetic: the sweep
/// walks each list from the front and stops at the first price that does not fire, which is what
/// makes the scan cost proportional to how far the mark moved instead of to how many algo orders
/// exist. Get a direction backwards and the sweep either fires nothing or walks everything.
mod algo_trigger_index;

/// `drainDeferredWork` — the permissionless drain for work a capped sweep left behind.
mod deferred_work_drain;
