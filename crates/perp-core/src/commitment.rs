//! Per-block chained off-trie commitment (catalog #16d): the off-trie analogue of the
//! state root. The precompile shell folds this once at block end and anchors the result
//! in 0x1003's single on-trie storage slot.

use context_interface::journaled_state::PerpDelta;
use primitives::{B256, U256};

/// Version byte mixed into the per-block commitment hash (catalog #16d). Bumped to 3 at the
/// switch from the per-call chained v2 (retired) to the per-block net-delta fold; bumped to 4
/// at the switch from keccak-derived storage keys to direct-packed keys (catalog #12), so the
/// two key framings never alias across the consensus transition (a devnet wipe accompanies the
/// bump); bumped to 5 at the price-index switch from sorted `Vec<u64>` to `Vec<u64>`
/// (catalog #22) — the serialized price-level bytes change (container + order); bumped to 6 at the
/// order-lifecycle redesign (commit-only #23): delete-on-terminal removes filled/cancelled orders
/// from the map, the new per-level live-order count + lazy FIFO change the level-key set, and the
/// signed-order replay guard moved to the seen-signature namespace — all shift the net-delta bytes;
/// bumped to 7 grouping the five per-market hot scalars (mark price, best bid/ask, last traded,
/// open interest) into one `MarketHot` blob — the delta keys + framing regroup (values identical);
/// bumped to 8 folding the two per-user scalar keys (fee-rate bps, order nonce) into the account
/// blob — the account blob grows and their standalone keys disappear from the delta; bumped to 9
/// moving mark_price out of `MarketHot` into the `Market` blob (write-rare + co-read with config) —
/// both blobs' bytes change (Market gains a field, MarketHot loses one); bumped to 10 folding each
/// level's live-order count INTO its FIFO blob (`LevelBlob`, count(8 BE) prefix) — the per-level
/// count keys disappear and the level blob framing changes; bumped to 11 when total perp
/// collateral was appended to the account blob.
// #A: bumped 11→12 for the PerpPosition reservation-aggregate fields (tbq/tbn/tsq/tsn) — a CHAIN
// change (position blob layout changed → persisted state + commitment differ). Requires a golden
// re-pin (below) + a coordinated wipe on deploy.
// Margin tiers (Phase 1): bumped 13→14 for the `tiers` field appended to the `Market` blob — every
// stored market blob grows by its (default, single-tier) table. CHAIN change; behaviour is
// arithmetically identical (mmr = 1/(2*3) = 1/6 == the deleted MAINTENANCE_MARGIN_DENOMINATOR).
// fee_reserved removal: bumped 14→15 — `PerpPosition` lost its "fr" field.
// Isolated funding (A1): bumped 15→16 — funding now settles against `pos.margin` instead of the
// account-global perp wallet, so the same funding event writes a different position/account value
// set (and `liquidate_position` no longer writes the account at all). No layout change; this is an
// EXECUTION-RULE change, so the values folded into the delta differ. CHAIN change: golden re-pin
// (below) + a coordinated wipe on deploy.
// Per-user market index (derived-ooIM Phase 0): bumped 16→17 — a NEW off-trie namespace
// ("umkt", per-user set of the markets the user is active in) enters the block delta whenever a
// user enters or leaves a market. Purely ADDITIVE: no existing blob's layout or value changes and
// no execution rule moves (the golden BusinessSnapshot is unchanged), but the delta gains keys, so
// the commitment shifts. CHAIN change: golden re-pin (below) + a coordinated wipe on deploy.
// Derived open-order margin (Phase 2): bumped 17→18 — the open-order margin ESCROW is DELETED.
// `PerpPosition` loses six fields ("mr", "mrn", "br", "brn", "sr", "srn"), so every stored
// position blob shortens and its later fields shift; and placement/cancel/fill/setLeverage no
// longer move the wallet for a reservation, so the account values folded into the delta differ
// too. Both a LAYOUT and an EXECUTION-RULE change. The open-order requirement is now DERIVED on
// read (`ooIM = ROUND_UP(max(|N + Bid|, |N − Ask|) / L) − ROUND_UP(|N| / L)`) and merely
// subtracted at the admission gate. CHAIN change: golden re-pin (below) + a coordinated wipe on
// deploy.
// Assuming-Price sell side + M1′ maker fills: bumped 18→19 — TWO execution-rule changes, no layout
// change. (1) `Ask` is now priced at each sell's ASSUMING PRICE `max(ROUND_UP(lastTraded × 1.0015),
// mark, limit)` instead of its limit price, so admission accepts/refuses a different set of resting
// sells (`math::assuming_price_floor`). (2) A maker fill whose wallet cannot cover the opening margin
// now FILLS and drives `perp_wallet_balance` negative instead of being cancelled, so the write set of
// such a match differs (a position + a negative wallet where there used to be a cancelled order).
// Both change what nodes write, so a node on 18 and a node on 19 would diverge on state — which is
// exactly what this version guards. CHAIN change: golden re-pin (below) + a coordinated wipe on
// deploy.
// M1 maker fills (the deficit lands in the SILO, not the wallet): bumped 19→20 — ONE execution-rule
// change, no layout change. An underfunded maker fill now funds its opening leg with
// `min(opening_value / L, cash at hand)` and lets `pos.margin` be SHORT by the rest
// (`settlement::OpeningMarginFunding::CappedAtCashAtHand`), where 19 funded the silo in full and
// drove `perp_wallet_balance` NEGATIVE by the shortfall (model M1′, which 19 adopted from a
// conjecture the docs marked ~50/50). R11 measured Binance doing the former
// (`derived-ooim-plan.md` §3a: `isolatedWallet == W0 + realized` digit-for-digit against a higher
// IM-implied figure). The write set of such a match differs — a thinner position blob and a
// non-negative account blob where 19 wrote a full silo and a negative wallet — and the divergence
// PROPAGATES: the thinner silo absorbs less of a later close's loss, so a different amount reaches
// the insurance fund. A node on 19 and a node on 20 would disagree on state, which is exactly what
// this version guards. CHAIN change: golden re-pin (below) + a coordinated wipe on deploy. (The
// golden SCENARIO never underfunds a maker, so its write set is byte-identical at 19 and 20 and its
// BusinessSnapshot is unchanged; the golden value moves only because this byte is hashed into it.)
// Frozen per-order Assuming Price (R12): bumped 20→21 — BOTH a layout and an execution-rule change.
// `OrderEntry` gains a trailing "ap" field (`assuming_price`), so EVERY per-user order-list blob
// grows by one integer; and a resting order's contribution to `Bid`/`Ask` is now the value frozen
// when it was placed instead of being re-derived at every read from the CURRENT
// `T = max(ROUND_UP(lastTraded × 1.0015), mark)`. `total_sell_notional` therefore carries the
// markup (it was the limit-price baseline before), which changes the value written for any user with
// a marked-up resting sell, and changes admission arithmetic: a later print or mark move no longer
// re-prices an order that is already resting. MEASURED — `misc/binance-flip-and-admission.md` §3.13
// (R12): 90 frames, the reported `askNotional` never moved, `H_live` refused by 1939 quanta with 9
// consecutive frames below the kink. `N = |position| × mark` stays LIVE, so `ooIM` still moves with
// the mark (R10) — only the per-order terms froze. A node on 20 and a node on 21 would disagree on
// both the blob bytes and the admission verdict, which is exactly what this version guards. CHAIN
// change: golden re-pin (below) + a coordinated wipe on deploy.
// Out-of-band resting-order expiry: bumped 21→22 — ONE execution-rule change, no layout change.
// `updateIndexPrice` now, after persisting the new mark and before the liquidation sweep, EXPIRES the
// contiguous NEAR-SIDE prefix of levels the moved band has stranded (asks below the lower edge, bids
// above the upper edge), capped at `MAX_BAND_EXPIRIES_PER_UPDATE` orders per update. Such a level was
// unmatchable indefinitely yet WAS `best_bid`/`best_ask`, so it made the PostOnly cross check reject
// legitimate orders, polluted the price-basis window and therefore the mark median, and was re-skipped
// by every match walk. The write set of an oracle update over a stranded book differs — order records
// deleted, level counts decremented, price-index and per-user order-list blobs shrunk, position
// aggregates lowered — and it PROPAGATES: a different BBO feeds a different `price2`, so subsequent
// marks (and every liquidation decision taken against them) diverge. A node on 21 and a node on 22
// would disagree on state, which is exactly what this version guards. `OrderCancelled` also gains a
// `uint8 reason` (its topic0 moves), so a consumer keyed to the 21 event must not silently decode 22
// logs. CHAIN change: golden re-pin (below) + a coordinated wipe on deploy. (The golden SCENARIO
// disables the band — `priceBandBps: 1_000_000` — so no level is ever out of band in it, the sweep is
// a no-op there, its write set is byte-identical at 21 and 22 and its BusinessSnapshot is unchanged;
// the golden value moves only because this byte is hashed into it. VERIFIED, not assumed: the whole
// suite including that pin passed at the OLD value with the new rule in place and this byte still 21.)
// Stored `Σ pos.margin` on the account blob: bumped 22→23 — a LAYOUT change plus a change to WHICH
// KEYS a position write touches; no execution rule moves. `UserAccount` gains a trailing "PM" field
// (`total_position_margin` = `Σ pos.margin` over the user's positions, i.e. Binance's
// `totalWalletBalance − totalCrossWalletBalance`), so EVERY stored account blob grows by one integer;
// and `storage::save_position` now maintains it from the delta of the position write, which means a
// write that MOVES `pos.margin` (an open, a fill, a close, funding, liquidation, ADL,
// `add/removePositionMargin`) also writes the ACCOUNT key — a key that write did not previously enter
// into the block delta. A rest/cancel is untouched: it goes through
// `save_position_reservation_only`, which cannot change `margin` (asserted), so a pure placement and a
// pure cancel still write no account key.
//
// What this buys: `AccountBalanceChanged.totalWalletBalance` used to be WALKED — one index load plus
// one position load per member market, up to 17 `_ref` loads at `MAX_USER_MARKETS` — on every
// published snapshot, i.e. on the fill path. It is now one account load and one addition. The
// increment itself is free: `save_position` already reads the old position for its zero-crossing
// hooks, so `old.margin` costs nothing.
//
// No BUSINESS behaviour changes — every published number is the same quantity, and the account blob's
// existing fields are untouched — but the block's net key→value delta differs (longer account blobs,
// plus account keys on the margin-moving position writes), so a node on 22 and a node on 23 would
// disagree on the commitment. CHAIN change: golden re-pin (see `trading::tests::golden`) + a
// coordinated wipe on deploy. The golden scenario's BusinessSnapshot is UNCHANGED, field for field.
// Cumulative realised PnL per position (`ACCOUNT_UPDATE.a.P[].cr`): bumped 23→24 — a LAYOUT change
// only; NO execution rule moves and no money is routed differently. `PerpPosition` gains a trailing
// "cr" field (`cumulative_realized_pnl`), so EVERY stored position blob grows by one integer, and
// every close now writes a position key with a different trailing value than it used to. Positional
// msgpack means a trailing field is the only safe place to add one (`codec::encode`), and it is
// where this one is.
//
// Why it has to be STORED rather than derived: it is the running sum of the per-closing-leg
// `realized_pnl` that `PositionChanged` already publishes as a per-event DELTA, and it must SURVIVE
// the position going flat and being reopened. Nothing in `(amount, v_quote_balance, margin)` retains
// a closed round's PnL, so there is no state to reconstruct it from — which is precisely the reason
// the field was carried as a hardcoded `0` placeholder on the event until now.
//
// ONE accumulation site, the single place a closing leg's PnL is computed:
// `settlement::apply_position_fill` (taker, maker, both ADL legs). `checked_add`, never saturating.
// There used to be a second — a liquidation residual closed at mark against no counterparty — but
// every residual is now an ADL fill and goes through the one site.
//
// No BUSINESS behaviour changes — nothing reads the field back to make a decision, no balance
// identity contains it, and no gate conditions on it — but every position blob's bytes differ, so a
// node on 23 and a node on 24 would disagree on the commitment. CHAIN change: golden re-pin (see
// `trading::tests::golden`) + a coordinated wipe on deploy. The golden scenario's BusinessSnapshot is
// UNCHANGED, field for field (it pins `getPosition`'s `(amount, vQuoteBalance, margin)` triples and
// `getAccount`'s `availableBalance`, none of which carries this field).
// ── 27 ───────────────────────────────────────────────────────────────────────────────────
// New per-market off-trie key `idxm` ([`crate::types::IndexModeState`]): the book-derived index
// EWMA accumulator plus the external-feed handover ramp, added for `updateIndexPriceFromBook`.
//
// It lands in the delta of EXISTING scenarios, not only new ones: `updateIndexPrice` writes the
// accumulator too (`index_scaled = index_price * INDEX_SCALE`) so that the book path never needs
// a seeding special case, so every block containing an oracle push now carries one more key.
//
// No BUSINESS behaviour changes on the external path — the exposed `index_price` is still exactly
// the oracle's argument (the accumulator is derived FROM it, never the reverse) unless a handover
// ramp is in flight, which requires a prior `updateIndexPriceFromBook`. CHAIN change: golden
// re-pin + a coordinated wipe on deploy. The golden scenario's BusinessSnapshot is UNCHANGED,
// field for field.
// ── 28 ───────────────────────────────────────────────────────────────────────────────────
// `Market` gains `basis_mode` (positional msgpack, appended), selecting which mechanism feeds
// price2 of the mark. Every existing market defaults to `BASIS_MODE_WINDOW` — the 30-second
// time-weighted window it already used — so the USD-M markets, which are aligned to Binance on
// this, are byte-identical in behaviour. Only the Market blob's SERIALISATION changed, by one
// trailing field, which is why the commitment moves at all.
//
// No BUSINESS behaviour changes: the golden scenario's BusinessSnapshot is UNCHANGED, field for
// field. CHAIN change: golden re-pin + wipe on deploy.
// ── 32 ───────────────────────────────────────────────────────────────────────────────────
// `removePositionMargin` takes Binance's isolated withdrawal cap (roadmap 2b):
// `max(0, min(iw − MM, iw + uPnL − PIM))`, refused whole above it. ONE execution-rule change, no
// layout change. The rule it replaces asked only `amount ≤ iw` and `iw − amount + uPnL ≥ MM` (both
// kept, the latter as the liquidation guard), so 32 is a pure tightening: a node on 31 accepts what
// a node on 32 refuses (never the reverse), and every balance and liquidation decision downstream
// of that withdrawal diverges. The golden scenario's one withdrawal (Phase 8: 250_000 back out of
// the 500_000 added in Phase 7, after a funding settle) sits inside the new cap, so its write set
// and its BusinessSnapshot are byte-identical at 31 and 32, and the golden value moves only because
// this byte is hashed into it. VERIFIED, not assumed: the whole golden pin passed at the OLD value
// with the new rule in place and this byte still 31. CHAIN change: golden re-pin + a coordinated
// wipe on deploy.
// ── 33 ───────────────────────────────────────────────────────────────────────────────────
// A funding charge comes out of the position's EQUITY: the whole payment lands on `pos.margin`,
// which may now go NEGATIVE (a debt), instead of stopping at 0 and drawing the remainder from the
// insurance fund on the spot. Three execution-rule changes land together, because the first alone
// would strand or mint the debt:
//   * funding: `margin += payment`, either sign, no clamp, no insurance-fund leg;
//   * a close releases margin SIGNED (`apply_position_fill`), so the closed slice nets its share of
//     a debt against its PnL, and a full close leaves 0 — never a debt on a flat position;
//   * ADL settles each WINNER's pending funding before its fill (and ranks candidates on margin
//     including it); a short whose debt outgrew its entry proceeds gets bankruptcy price 0 instead
//     of an `Err` (`calc_bankruptcy_price` no longer errors on any representable position), and
//     fills at one price unit like the sub-unit regime.
// No layout change. The golden scenario's one funding charge (400) is far inside its margin and its
// ADL winners carry no pending funding, so its write set and BusinessSnapshot are byte-identical
// at 32 and 33 — VERIFIED by running it with all three changes in place against the old pin
// before bumping this byte. CHAIN change: golden re-pin + a coordinated wipe on deploy.
// ── 34 ───────────────────────────────────────────────────────────────────────────────────
// Sub-accounts + inter-account transfer (P1). A LAYOUT change: `UserAccount` gains two trailing
// fields — "MA" (`master: Option<Address>`, set only on a sub) and "SC" (`sub_count: u8`, set only
// on a master) — so EVERY stored account blob grows by at least two bytes (`nil` + fixint 0) and a
// sub's by 22 + 1. Positional msgpack, appended at the tail like every earlier account field.
//
// New EXECUTION rules land with it, all on new selectors (`createSubAccount`, `asSubAccount`,
// `internalTransfer`) except one: `setUserFeeRates` now refuses a sub and pushes a master's rates
// down to each of its subs. No new key prefix — a sub is an ordinary account keyed by its derived
// address (`types::derive_sub_account`), and the family is the two fields above.
//
// No BUSINESS behaviour changes for any pre-existing flow: the golden scenario creates no sub and
// makes no internal transfer, so its write set differs only in the account blobs' two extra bytes,
// and its BusinessSnapshot is UNCHANGED, field for field. VERIFIED, not assumed: with the two
// fields `#[serde(skip)]` and this byte at 33, the golden reproduced the old pin 0x913b6760…40ce
// exactly. CHAIN change: golden re-pin + a coordinated wipe on deploy.
// ── 35 ───────────────────────────────────────────────────────────────────────────────────
// `cancelAlgoOrderSigned` burns its signature on submission, like the other eight signed
// selectors: one more seen-signature write per call (and per rejected call, as a sanctioned
// retained write). ONE execution-rule change, no layout change. A replay is now refused as a
// duplicate instead of reaching the cancel. The golden scenario sends no signed algo cancel, so
// its write set and BusinessSnapshot are byte-identical at 34 and 35; only this byte moves the
// golden. CHAIN change: golden re-pin + a coordinated wipe on deploy.
// ── 36 ───────────────────────────────────────────────────────────────────────────────────
// The self-trade-prevention series. ONE bump covers the whole series; each commit that moves the
// golden re-pins it and adds its line here.
//   * Per-market open-order cap (`algo::MAX_OPEN_ORDERS_PER_MARKET` = 200, Binance's
//     `MAX_NUM_ORDERS`): book buys + book sells + untriggered conditionals per (user, market). A
//     GTC / PostOnly placement or a `placeAlgoOrder` at the cap is refused pre-write; IOC / FOK /
//     market orders, liquidation closes and fired triggers are never refused by it. A pure
//     tightening (a node on 35 accepts what a node on 36 refuses, never the reverse), no layout
//     change. The golden scenario never comes near 200 orders, so its write set and
//     BusinessSnapshot are byte-identical; only this byte moves the golden.
//   * STP encoding: `Order` and `OrderEntry` gain a trailing `stp` (`StpMode`, positional msgpack,
//     appended), so every order blob and every per-user entry list grows by one byte per order.
//     `flags` bits 3:2 are decoded (`0b11` refused), stored and echoed; matching does not read the
//     field yet. A LAYOUT change; BusinessSnapshot unchanged.
//   * A triggered conditional whose ordinary placement the matcher REFUSES (unfillable FOK,
//     crossing PostOnly, refused rest) is deleted with `AlgoOrderCancelled(TriggerRejected = 10)`
//     instead of reverting the oracle transaction over the trigger's retained writes. An
//     execution-rule change (a node on 35 reverts that tick, a node on 36 commits it); the golden
//     scenario fires no trigger, so its pin does not move.
//   * A match participant whose only part was LOSING an order (published nothing; `amount`,
//     `margin` and the account blob as stored) is flushed through the reservation-only position
//     write: no `AccountBalanceChanged`, and no same-value account write in the block delta. The
//     golden scenario has no such participant, so its pin does not move.
//   * STP matching: a GTC / IOC / market taker that meets a resting order of its OWN address no
//     longer trades with it. The taker's mode decides: EXPIRE_MAKER cancels that order (reason 9)
//     and walks on; EXPIRE_TAKER stops and expires the remainder (`OrderExpired.reason` 9, a GTC
//     included — it never rests); EXPIRE_BOTH does both. FOK, PostOnly and liquidation closes are
//     unchanged. An execution-rule change: a node on 35 self-trades where a node on 36 cancels or
//     expires. The golden scenario has no same-address cross, so its write set, pin and
//     BusinessSnapshot are unchanged.
// ── 37 ───────────────────────────────────────────────────────────────────────────────────
// Trading fees in ppm, and the maker rebate. ONE bump covers the series; each commit adds its line.
//   * Trading-fee rates move from basis points to PARTS PER MILLION (`math::FEE_PPM_DENOMINATOR`),
//     and the maker rate becomes SIGNED: `UserAccount` "MF" and `OrderEntry`'s snapshot (renamed
//     "MFB" → "MFP" — positional msgpack, so the name is not on the wire) are `i64`, "TF" stays
//     `u64`. A LAYOUT change by VALUE, not by shape: a non-negative `i64` encodes exactly like the
//     `u64` it was, but every stored non-zero rate is now 100× larger, so its msgpack integer can
//     widen (2 bps = fixint `0x02` → 200 ppm = `0xcc 0xc8`). Positive-rate arithmetic is
//     byte-identical — `floor(n·100r / 1e6) == floor(n·r / 1e4)` — so no fee, balance or fee total
//     moves. At this step negative maker rates were still refused; the rebate bullet below lifts
//     that (`maker ≥ −R`). The golden scenario's rates are rescaled ×100 (2/5 bps → 200/500
//     ppm); its account and order-entry blobs widen, and its BusinessSnapshot is UNCHANGED, field
//     for field. CHAIN change: golden re-pin + a coordinated wipe on deploy.
//   * The maker REBATE. A new global key `mrbt` (the bound `R`, u64 ppm, absent = 0) and its
//     setter; `setUserFeeRates` accepts `maker ≥ −R`. A fill against a negative-rate resting order
//     credits that maker `min(floor(n_i·|m|/1e6), share_i)` — `share_i` the fill's cumulative-floor
//     share of the taker fee — to its perp wallet; the fee recipient is credited the taker fee NET
//     of the match's rebates, and `market_fee_total` grows by the same net amount. An
//     execution-rule change (a node on 36 refuses a negative rate a node on 37 accepts, and pays
//     no rebate), but only reachable once the admin raises `R`: with no negative rate anywhere every
//     write is byte-identical. The golden scenario sets none, so its pin and BusinessSnapshot do
//     not move.
// ── 38 ───────────────────────────────────────────────────────────────────────────────────
// Conditional orders: the fire() capacity fix, closePosition parity and the trailing stop. ONE
// bump covers the series; each commit adds its line.
//   * A fired conditional order is sized by the SAME capacity rule as `placeOrder`'s reduce-only
//     admission (`reduce_only::closing_capacity`). One that fills immediately — the `_MARKET` pair,
//     or a limit IOC / FOK — is bounded by `|position|` alone; it used to have the owner's resting
//     reduce-only orders (and the normal orders "ahead of the mark") subtracted too, so a stop
//     beside a full-size reduce-only take-profit closed nothing and expired (reason 6), and beside
//     a half-size one closed only half. The resting reduce-only orders it leaves over-committed are
//     evicted after the fill by the ordinary pass (Binance `er=4`). A fired order that RESTS (GTC /
//     PostOnly) keeps the prefix condition at its own limit price. An execution-rule change (a node
//     on 37 expires a stop that a node on 38 fills), no layout change. The golden scenario fires
//     no trigger, so its write set and BusinessSnapshot are byte-identical — VERIFIED: with the fix
//     in place and this byte still 37 the scenario reproduced the old pin 0x525fea15…33ee. Only
//     this byte moves the golden. CHAIN change: golden re-pin + a coordinated wipe on deploy.
//   * closePosition parity. A LAYOUT change: `PerpPosition` gains a trailing "g" (`generation`,
//     u64 — bumped by `save_position` each time a non-zero `amount` goes to 0 or changes sign) and
//     `Order` a trailing "pg" (`position_generation`, u64), positional msgpack, appended — so every
//     position blob and every order blob grows by at least one byte. Three placement rules land
//     with it, each a pure tightening (a node on 38 refuses what a node on 37 accepted, never the
//     reverse): `placeAlgoOrder` refuses reduceOnly together with closePosition; refuses any
//     conditional whose trigger the STORED mark already satisfies ("would immediately trigger",
//     the sweep's own inclusive predicate; skipped at mark 0, unreachable through the ABI); and
//     refuses a closePosition order unless a position is open with the order on its closing side,
//     recording its generation. And one execution rule: a fired closePosition order whose recorded
//     generation differs from the position's is spent with `AlgoOrderCancelled(PositionClosed =
//     11)` and places nothing (it used to close whatever was open, or expire as 6 after a flip).
//     The golden scenario sends nothing these refuse and fires no trigger, so its write set differs
//     only by the two fields' bytes — VERIFIED: with both `#[serde(skip)]` it reproduced the
//     previous 38 pin 0x720411d8…e3e0. BusinessSnapshot UNCHANGED. CHAIN change: golden re-pin +
//     a coordinated wipe on deploy.
//   * TRAILING_STOP_MARKET (`orderType = 4`). A LAYOUT change: `Order` gains a trailing "tb"
//     (`trail_bps`, u16, 0 = not trailing), so every order blob grows by one byte. Seven NEW key
//     families (`trlm` meta, `trls` stack pages, `trld` directory, `trlb` buckets, `trlo` links,
//     `tpnp`/`tpnl` pending index; all raw packed bytes) enter the delta of any block that places,
//     activates, fires or cancels a trailing stop, and every oracle tick of a market that has one.
//     New execution rules, all reachable only through the new order type: the unbudgeted stack
//     step, the trailing fire walk on the shared sweep budget after the fixed triggers, the
//     activation walk on its own 64 cap, and `AlgoOrderActivated`. A node on the previous 38
//     refuses `orderType = 4` that this node accepts. The golden scenario places no trailing stop,
//     so its write set differs only by the "tb" byte (each oracle tick reads the absent `trlm` and
//     writes nothing) — VERIFIED: with the field `#[serde(skip)]` it reproduced the previous 38 pin
//     0xa72ce19d…d940. BusinessSnapshot UNCHANGED. CHAIN change: golden re-pin + a coordinated
//     wipe on deploy.
// ── 39 ───────────────────────────────────────────────────────────────────────────────────
// CROSS margin mode (Binance USDⓈ-M parity), phases P0–P4. ONE bump covers the series; each
// commit that moves the golden re-pins it and adds its line here.
//   * P0 — explicit maintenance-margin-rate column. A LAYOUT change: `MarginTier` gains a
//     trailing `maint_margin_rate_ppm` (u32, positional msgpack, appended, serde default; `0` =
//     DERIVED `1/(2·maxLeverage)`, the only rule before), so every tier row in every `Market` blob
//     grows by one byte (a fixint 0). Execution rules, all reachable only by sending a non-zero
//     column through `setMarginTiers` (whose ABI gains the column, so its selector moves): a set
//     rate replaces the derived one inside the unchanged slice form, `⌊slice · ppm / 1e6⌋` per
//     band; the table is refused unless every effective rate is strictly below `1/maxLeverage` and
//     the effective rates are non-decreasing (a pure tightening of what a zero column could
//     express — a node on 38 has no column to refuse). `MAX_MARGIN_TIERS` 8 → 16 (the codec emits
//     only live rows, so no blob changes for it). With a zero column `maintenance_margin` is the
//     same expression byte for byte. The golden scenario installs no table, so its write set
//     differs only by the market blobs' extra byte — VERIFIED: with the field `#[serde(skip)]` it
//     reproduced the previous 38 pin 0x37a18141…79de. BusinessSnapshot UNCHANGED. CHAIN change:
//     golden re-pin + a coordinated wipe on deploy.
//   * P1 — margin type, gated off. A LAYOUT change: `PerpPosition` gains a trailing "mt"
//     (`margin_type`, u8, positional msgpack, appended, serde default; 0 = CROSSED, Binance's
//     default, 1 = ISOLATED), so every position blob grows by one byte (a fixint). No execution
//     rule moves: `CROSS_MARGIN_ENABLED = false`, so every position is ISOLATED in force whatever
//     the byte says, and no margin rule reads it. Only reporting carries it (the effective type
//     on `AccountPosition`, `PositionChanged`, `getSymbolConfig`; logs are not committed). The
//     golden scenario's write set differs only by the position blobs' extra byte — VERIFIED: with
//     the field `#[serde(skip)]` it reproduced the 39-P0 pin 0xafd82645…7b48. BusinessSnapshot
//     UNCHANGED. CHAIN change: golden re-pin + a coordinated wipe on deploy.
//   * P1 — `setMarginType` / `setMarginTypeSigned` (and `setMarginType` on the `asSubAccount`
//     whitelist). New selectors, no layout change. While the gate is off neither can write the
//     type byte — ISOLATED is already in force (`-4046`) and CROSSED is refused — so the only
//     state they can move is the signed path's spent-signature marker (burned on submission,
//     like every signed selector). The golden scenario calls neither: its pin does not move.
//   * P0 review — `MAX_LEVERAGE_HARD_CAP` 100 → 150, so Binance's current BTCUSDT table (150x
//     first bracket) loads as published. A WIDENING of `setMarginTiers` validation (a node on the
//     previous build refuses a table with a tier above 100x that this one accepts), and through
//     the table, of `setLeverage`'s cap on such a market. No layout change; the golden scenario
//     installs no table: its pin does not move.
//   * P0 review — the terminal `notionalCap` (Binance's last-bracket cap, its maximum position).
//     `MarginTiers` gains `notional_cap` (0 = unbounded), carried as a FOURTH positional element of
//     the table's LAST row and only when non-zero, so an uncapped table's blob is byte-identical.
//     `setMarginTiers` takes it (selector moves again), `MarginTiersUpdated` carries it (topic0
//     moves again). New execution rules, reachable only through a non-zero cap: an opening fill
//     leaving `|notional| >= cap` is refused (taker revert `-2027`, maker order cancelled like a
//     tier refusal), and `setLeverage` on a position that large is `-2027`. The golden scenario
//     installs no table: its pin does not move.
//   * P2 — cross accounting, layout. `Market` gains a trailing "fx" (`cumulative_funding_index`,
//     i128, positional msgpack, appended, serde default 0): a MIRROR of `FundingState`'s index,
//     written by `save_funding_state` (the one door of the index) whenever it differs, so an
//     account fold prices pending funding off the `Market` it already loads. The `Market` blob is
//     rewritten on every oracle tick anyway (`save_mark_price`), so the mirror adds no key to any
//     block's delta. `UserAccount` gains trailing "LF" (`liq_flags`, u8, bit 0 = IN_LIQUIDATION —
//     defined, set by nothing before P3) and "OP" (`open_position_count`, u8: positions with
//     `amount != 0`, cross and isolated, maintained by `save_position` on the zero-crossing it
//     already detects, inside the account write an open or close already makes; `save_account`
//     carries both over from the store, like `total_position_margin`). No execution rule moves
//     while the gate is off. The golden scenario's write set differs only by the blobs' extra
//     bytes — VERIFIED: with all three fields `#[serde(skip)]` it reproduced the 39-P1 pin
//     0x7deaa026…6637. BusinessSnapshot UNCHANGED.
//   * P2 review fixes — execution rules for positions CROSSED in force only: a pure reduce on a
//     cross wallet is not margin-gated, a zero-draw taker close walks no account fold (J4), the
//     withdrawable balance `X` gains the maintenance floor `E − Σ_I ooIM − Σ_C MM`, a resting cross
//     order pays its Open Loss, and a cross order's margin refusal carries `-2019`. Gated off no
//     write, event or refusal of an isolated account moves: the golden pin 0x8c500d81…a62c is
//     unchanged, BusinessSnapshot UNCHANGED.
//   * P3 — the liquidation clearance fee (owner decision Q3, J5, R7). An EXECUTION-RULE change for
//     isolated liquidations, no layout change: the fee is `notional(mark, closed quantity) ×
//     liquidation_fee_rate_bps / 10_000` — a rate on the NOTIONAL the call closed, Binance's
//     `liquidationFee` — where it was the same rate on the silo margin the close consumed, and it is
//     capped at the close credit the close returned to the wallet (and at the positive wallet), so
//     it can never draw on the cross pool and a bankrupt liquidation pays nothing. The fee moves a
//     different amount from the wallet to the insurance fund, so the account and fund values
//     folded into the delta differ. The INTENDED isolated business change: the golden scenario's
//     BusinessSnapshot moves in exactly two fields, by the same 6_046 — ALICE's available balance
//     (999_059_631 → 999_053_585) and the insurance fund (49_007_954 → 49_014_000): her fee is
//     14_000 (50 bps of 0.04 @ $70) instead of 7_954 (50 bps of her silo). Golden re-pinned
//     0x8c500d81…a62c → 0xaea562b9…1e61.
//   * P3 — cross liquidation (the account-level episode, `perp_engine::risk::cross_liquidation`):
//     the sweep and `liquidate()` judge a position CROSSED in force on its ACCOUNT (`E < Σ_C MM`)
//     and run the episode — `liq_flags` set and cleared, every order of the account cancelled,
//     each cross market closed by a limit IOC at the MM-share bankruptcy price and ADL, a
//     continuation recorded in `Market::deferred_work`. New execution rules reachable only on a
//     position CROSSED in force: gated off no write of an isolated account moves; the golden pin
//     0xaea562b9…1e61 is unchanged, BusinessSnapshot UNCHANGED. (The `Liquidation` event's
//     appended `marginType` is a log, not state.)
//   * P3 — negative-balance clearance (owner decisions Q8, R4). An EXECUTION-RULE change for both
//     margin types, no layout change: an account whose LAST position closes (any route) with the
//     wallet below zero has it cleared at the end of that call — the insurance fund pays `−W` (what
//     it has; the rest is socialised, `InsuranceFundDepleted`) and the wallet becomes 0. Isolated
//     accounts reach it too (a maker commission on a last close was the one route). The golden
//     scenario leaves no account flat below zero: the pin 0xaea562b9…1e61 is unchanged,
//     BusinessSnapshot UNCHANGED.
//   * P3 — the liquidation flag ends with the account's last cross position, whatever closed it
//     (a reduce-only close, a maker fill, a fired stop, ADL against the account): the end of that
//     call clears `liq_flags`. Reachable only on an account in cross liquidation: the golden pin
//     0xaea562b9…1e61 is unchanged, BusinessSnapshot UNCHANGED.
//   * P3 — cross ADL, final rules (R5): a CROSSED winner ranks `uPnL / E` (gain) or
//     `uPnL · E / N²` (loss) and is clean per fill while `E − take · |fill − mark| ≥ 0`; the rank
//     comparison is exact (512-bit cross-multiplication) for every winner. An ISOLATED winner's
//     ROE order is the one the saturating comparator gave wherever it did not saturate, so no
//     isolated ADL moves: the golden pin 0xaea562b9…1e61 is unchanged, BusinessSnapshot UNCHANGED.
//   * P3 — THE FLIP (R8): `CROSS_MARGIN_ENABLED = true`. An EXECUTION-RULE change, no layout
//     change: the stored margin type is now in force, so a position whose owner never chose —
//     stored 0, Binance's default — is CROSSED (the shared cross wallet, no silo, account-level
//     liquidation), and `setMarginType(CROSSED)` succeeds after the §3.6 checks. The golden
//     scenario pins ISOLATED margin, so its three traders now choose ISOLATED explicitly
//     (`setMarginType` on the flat market before any order): their position records store 1
//     where they stored 0, the only difference in the write set — VERIFIED: with the gate forced
//     off and the three switches skipped the scenario reproduced the previous pin
//     0xaea562b9…1e61. BusinessSnapshot UNCHANGED (the flip moves none of it; R7's earlier
//     two-field delta stands). Golden re-pinned 0xaea562b9…1e61 → 0xfdf928b5…47d4. CHAIN change:
//     a coordinated wipe on deploy.
//   * P3 — the CROSS golden scenario (`perp_engine::cross_golden_tests`), a second pin beside the
//     isolated one, of cross margin as it ships: mixed cross / isolated accounts, a funding epoch,
//     a partial cross liquidation, a bankrupt episode ADL-ed at the MM-share bankruptcy price
//     past a stale leg, the flag ended by a reduce-only close and the negative wallet that close
//     left cleared by the insurance fund. Pinned 0x84977f13…e969 with its own business snapshot.
//     A test only: no rule moves, and the isolated pin 0xfdf928b5…47d4 is unchanged.
//   * P3 review — ADL is TOTAL on fill-price arithmetic (J4): an opposite holder that cannot be
//     valued (at the fill price above all) is fallback tier instead of an `Err` that halted the
//     sweep, and a fill that cannot be valued is skipped. Reachable only past the integer widths:
//     both pins unchanged (0xfdf928b5…47d4, 0x84977f13…e969), BusinessSnapshot UNCHANGED.
//   * P3 review — a CROSSED loser's ADL price (`E < 0`) is clamped into the market's band, like
//     the episode IOC's limit; what the band's edge does not transfer stays in its wallet (its
//     silos net it, Q8 clears the rest). Reachable only in a cross episode: both pins unchanged,
//     BusinessSnapshot UNCHANGED.
//   * P3 review — a CROSSED ADL winner in the FALLBACK tier is backed by the insurance fund: its
//     `E` falls to `min(E, 0)` and no further, the rest is its bad debt for the fund (the bound an
//     isolated fallback winner's silo always gave). Reachable only with a cross winner: both pins
//     unchanged, BusinessSnapshot UNCHANGED.
//   * P3 review — a stale leg sets no ADL price: while any cross leg of the account is one the
//     episode skips (stale, Q10; mark 0 or unpriceable, J4), a `E < 0` market's rest goes to ADL
//     at the MARK, the deficit waiting on the account. Reachable only in a cross episode: the
//     isolated pin 0xfdf928b5…47d4 is unchanged, BusinessSnapshot UNCHANGED. The CROSS golden
//     ADLs DAVE past exactly such a leg (M2, never published), so its M3 fill moves from $173.57
//     to the $180 mark: BOB +$64.30, the fund −$64.30 (DAVE's cleared deficit $35.70 → $100) —
//     the intended change, written down in `cross_golden_tests`. Cross pin re-pinned
//     0x84977f13…e969 → 0x9c7be213…4132.
//   * P3 review — a CROSSED maker fill that OPENS needs cash at hand (`W > 0` after the fill's
//     close leg), else it is refused like an isolated K9 (`MakerInsolvent`, the order cancelled):
//     a receivable or a cleared wallet can no longer open through an order resting from before.
//     Reachable only with a cross maker at `W ≤ 0`: both pins unchanged (0xfdf928b5…47d4,
//     0x9c7be213…4132), BusinessSnapshot UNCHANGED.
//   * P3 review — an untriggered conditional order makes its market a member of the per-user
//     market index ("umkt"; the list is a fourth leg of the activity predicate, synced on every
//     `save_user_algo_orders`), so a cross liquidation's cancel-all reaches a conditional in a
//     market where the account holds nothing else; `placeAlgoOrder` is admitted under the
//     16-market cap. A WRITE-SET change: a conditional in a market not yet in the index adds the
//     id (and its retirement removes it). The goldens place none outside an indexed market: both
//     pins unchanged, BusinessSnapshot UNCHANGED.
pub const BLOCK_COMMITMENT_VERSION: u8 = 39;

/// Computes the per-BLOCK off-trie commitment over the block's NET delta (catalog #16d).
///
/// `C_block = blake3(C_prev ‖ BLOCK_COMMITMENT_VERSION ‖ Σ_sorted(key(32) ‖ len(u32 BE) ‖ value))`,
/// keys ascending. `delta` is the net block writes from [`JournalTr::take_perp_delta`] (already one
/// value per key, post-revert), so no coalescing is needed — only deterministic key-sorting (keys
/// are a total order; the HashMap is never iterated for the hash). An empty value is a deleted key,
/// framed with len 0 (same convention as the per-call path). Pure: the caller reads `C_prev` and
/// sstores the result. This commits the block's net STATE CHANGE; chained onto the previous block's
/// `C` it forms a block-granular running commitment, the off-trie analogue of the state root.
pub fn compute_block_commitment(prev: U256, delta: &PerpDelta) -> U256 {
    let mut keys: Vec<&B256> = delta.keys().collect();
    keys.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&prev.to_be_bytes::<32>());
    hasher.update(&[BLOCK_COMMITMENT_VERSION]);
    for key in keys {
        // Fold the canonical BYTES only — `decoded` never feeds the commitment (选项A), so the
        // byte-stream (and the on-trie anchor) is identical to the pre-Arc pipeline.
        let value = &delta[key].bytes;
        hasher.update(key.as_slice());
        hasher.update(&(value.len() as u32).to_be_bytes());
        hasher.update(value);
    }
    U256::from_be_bytes(*hasher.finalize().as_bytes())
}
