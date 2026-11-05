use super::*;
use crate::{
    interface::IPerpDex::{
        addMarketCall, addPositionMarginCall, cancelOrderSignedCall, depositCall,
        depositInsuranceFundCall, getAccountCall, getApiKeysCall, getAveragePremiumIndexCall,
        getBookLevelCall, getBookPricesCall, getFundingStateCall, getIndexPriceCall,
        getInsuranceFundCall, getMarkPriceCall, getOpenOrdersCall, getPositionCall,
        initAdminCall, liquidateCall, placeAlgoOrderCall, placeOrderSignedCall,
        registerApiKeyCall,
        removePositionMarginCall, revokeApiKeyCall, setLeverageCall, setLeverageSignedCall,
        setMarketManagerAddressCall, setOracleAddressCall, setUserFeeRatesCall,
        transferAdminCall, transferFromPerpCall, transferToPerpCall, updateIndexPriceCall,
        updateMarketCall, withdrawCall, withdrawInsuranceFundCall,
    },
    run_perp_dex_call,
    storage::keys as storage_keys,
};
use ed25519_dalek::{Signer, SigningKey};
use primitives::{b256, keccak256, Bytes, B256};

const ORACLE: Address = address!("4444444444444444444444444444444444444444");
const MANAGER: Address = address!("5555555555555555555555555555555555555555");
/// Fresh admin the scenario hands control to at the very end (transferAdmin).
const NEW_ADMIN: Address = address!("6666666666666666666666666666666666666666");

/// Pinned final commitment-slot value of `run_golden_scenario`.
/// Capture/re-pin procedure: run `commitment_golden_scenario` and copy the
/// `golden commitment =` line it prints (also shown in the assert diff).
/// Last re-pin 2026-06-14 (P4/#17 no-op skip): PriceBasisWindow re-store is
/// skipped when the observation changed nothing (same-block timestamp) — CHAIN
/// change (the skipped write leaves the commitment log), value re-pinned. The
/// window value is identical whether stored or skipped, so business SNAPSHOT
/// is unchanged.
/// (Prior re-pins: P4/#20 bin+enum 0x8a0b7f…; #20-positional 0xcdeac1…; #18
/// 0x58835a…; #24 0x995638…; 16b 0x69e699…; ext 0x2d5fa5…; P0.)
/// #16d (per-block commitment, v3): folded ONCE at block end from the net delta
/// (`take_perp_delta` → `finalize_block_commitment`), replacing the per-call v2 flush.
/// BusinessSnapshot is UNCHANGED (pure commitment-representation change). Prior v2 value
/// 0x3c80c530439970bd37d4bef6bcfd22411740241981764031154765a558bf9f47.
/// Price-band field (2026-07): `Market` gained `price_band_bps` (serialized as "pb"),
/// so every stored market blob is longer → write-stream commitment shifts. CHAIN change;
/// BusinessSnapshot UNCHANGED. Prior value
/// 0x24d9197680681d1b627f13e28ba971a3e1bf2589a979141ae48989efc797e7e6.
/// Band enforcement (2026-07): this scenario's market band was set to a disabled value
/// (1_000_000 bps) so the matching/settlement regression is unaffected by the new
/// placement band; the changed "pb" value shifts the blob → commitment. CHAIN change;
/// BusinessSnapshot UNCHANGED (band disabled → identical execution). Prior value
/// 0x05ec6b7a77d6bc57750d92e5624c5dd8262c291fdfa56da0b8fe1cd312bf6aed.
/// Position registry (2026-07, Phase B): save_position maintains a per-market
/// open-position registry ("preg" blob) on amount zero-crossings, so opening/closing a
/// position adds a registry write to the block net delta → commitment shifts. CHAIN
/// change; BusinessSnapshot UNCHANGED (the registry mirrors open positions and is read
/// by no view). Prior value
/// 0x4bad03218af966c0aa1b30a0611eebc746d7b9e07ec33115c2d14ae1fe833af3.
/// Liquidation fee waiver (2026-07, Phase C fix B): a liquidation close no longer
/// charges the liquidated user a taker trading fee (they pay only the clearance fee to
/// the IF), fixing an underwater user with no free wallet being un-liquidatable.
/// CHANGES BusinessSnapshot: ALICE keeps the taker fee she used to pay on her Phase-9
/// book close, and the market fee total drops by it. Prior value
/// 0xfcc04c1f1e41e7e284ace52730d47aaeb47955a3fb1ce27d1d52ea2aa34eab1e.
/// RE-PIN (catalog #12, direct-packed storage keys + `BLOCK_COMMITMENT_VERSION` 3→4): the
/// business snapshot is unchanged; only the key bytes folded into the commitment differ, so
/// this is a legitimate CHAIN re-pin (devnet wipe). Prior value
/// 0x9b493f51b6dd2011ae1c8b7ebc5a2b2b3b4fc2cc9fd1b46878d5d1a2b85e3435.
/// RE-PIN (catalog #22, price index Vec<u64>→BTreeSet<u64> + `BLOCK_COMMITMENT_VERSION` 4→5):
/// business snapshot unchanged; the serialized price-level bytes change (container + order).
/// Prior value 0x8bcfa8def86af905253c4de33a0ca43634683e94191de931bf9235a38be9597d.
/// RE-PIN (order-lifecycle redesign, commit-only #23 + `BLOCK_COMMITMENT_VERSION` 5→6):
/// delete-on-terminal removes filled/cancelled orders from the map (their blobs leave the net
/// delta, replaced by empty-value deletes), a new per-level live-order count key is folded in,
/// and the signed-order replay guard moved to the seen-signature namespace. The business
/// snapshot is UNCHANGED except that terminal orders are no longer queryable (getOrder reverts
/// → DELETED sentinel) — positions/accounts/fees/mark/funding are identical. Prior value
/// 0xcfa7fe64e534bd9adf65df5a2e4529b95754e150519352daeed5ce77d01f71c9.
/// RE-PIN (MarketHot grouping + `BLOCK_COMMITMENT_VERSION` 6→7): the five per-market hot
/// scalars (mark price, best bid/ask, last traded, open interest) moved from five single-scalar
/// keys into one grouped `MarketHot` blob, so the block-delta keys + framing regroup. Every
/// value — and the business snapshot — is IDENTICAL. Prior value
/// 0xcf5c23c371fd0661b74856c2d08e7df707b3b61831297a870e77276ae531a53f.
/// RE-PIN (per-user scalar fold + `BLOCK_COMMITMENT_VERSION` 7→8): the fee-rate bps + order
/// nonce moved from two standalone per-user keys into the account blob (those keys leave the
/// delta; the account blob grows). Values + business snapshot IDENTICAL. Prior value
/// 0x2d7a551bd8ef02c3d4933462314b1182be29f40fc284bf1b1c111eb1e423c956.
/// RE-PIN (mark_price → Market + `BLOCK_COMMITMENT_VERSION` 8→9): mark_price moved out of the
/// MarketHot blob into the Market blob (write-rare + co-read with config). Both blobs' bytes
/// change (Market gains a field, MarketHot loses one); values + business snapshot IDENTICAL.
/// Prior value 0x7288ee0d91edb642537fc632a47eb12d27542977b4ffb7bf97d6bf31a874336c.
/// RE-PIN (level count merge + `BLOCK_COMMITMENT_VERSION` 9→10): each level's live-order count
/// folded INTO its FIFO blob (`LevelBlob`, count(8 BE) prefix); the per-level count keys
/// disappear + the level blob framing changes. Values + business snapshot IDENTICAL. Prior
/// value 0x47f8225b07e2e1cc951203cc35b2aab40cc7801f43880c8ec3850344de3c78dc.
/// RE-PIN (event-authored account balances + `BLOCK_COMMITMENT_VERSION` 10→11): total perp
/// collateral was appended to the account blob and is now pinned alongside the exact available
/// balance. Prior value 0x25389cb16beb55f0800734570a0ac4d48f9f6309452932b7204fa682dd397d45.
/// RE-PIN (#A incremental-reservation aggregates + `BLOCK_COMMITMENT_VERSION` 11→12): the
/// `tbq/tbn/tsq/tsn` maintained totals were appended to the position blob (behavior-identical —
/// the business snapshot below is unchanged; only the persisted layout + commitment differ).
/// Prior value 0x9989c4d3675808defbb3baba3f828cb43151b6ffb81f0a902a2ef68286717648.
/// RE-PIN (drop the `total_perp_collateral` aggregate + `BLOCK_COMMITMENT_VERSION` 12→13): the
/// derivable "TC" field was REMOVED from the account blob (it was maintained on every hot write
/// but read by no protocol rule). Business snapshot below keeps every surviving value; only the
/// persisted layout, the getAccount/AccountBalanceChanged arity and the commitment differ.
/// Prior value 0x06ee401de8dd26982c5820f9263f67c349cb139ac9fc4d6b4fbfbb73a0e57a0e.
/// RE-PIN (margin tiers Phase 1 + `BLOCK_COMMITMENT_VERSION` 13→14): `Market` gained the
/// `tiers` table (appended last, serialised as "mt"), so every stored market blob grows by
/// its default single-tier `[{0, 3}]` row. Arithmetically BEHAVIOUR-PRESERVING — the tier
/// maintenance rate `1/(2*3)` is the deleted `MAINTENANCE_MARGIN_DENOMINATOR = 6` — and the
/// only rule change (leverage cap 6 → 3) was pre-absorbed by the Phase-0.5 retune of this
/// scenario, so the business snapshot below is UNCHANGED. Prior value
/// 0xc2c839a6a4dc5fa20b64faa286e30e6b90e7e7905ebe070c5c91bfd0bb2314f1.
/// RE-PIN (remove the `fee_reserved` escrow + `BLOCK_COMMITMENT_VERSION` 14→15): `PerpPosition`
/// LOST its "fr" field (every position blob shortens + every later field shifts), placement no
/// longer debits the order's prospective fee, and each fill charges the fee out of the margin
/// it funds (`fee_from_margin = min(fee, opening_margin)`). This CHANGES the business snapshot
/// — deliberately, it is the point of the change — in exactly three places, all conserving:
///   * `bob_account` 500_428_794 → 500_428_954 (+160): the 160 fee escrowed against BOB's still
///     resting tail bid is no longer withheld, so it sits in his AVAILABLE balance instead of
///     `pos.fee_reserved`. Pure bucket move — his total collateral is unchanged.
///   * `insurance_fund` 49_007_966 → 49_007_956 (−10) and `alice_account` 999_059_619 →
///     999_059_629 (+10): ALICE's opening taker fees now come out of her position margin, so
///     her pre-liquidation margin is 2_000 lower (1_593_200 → 1_591_200) and the 50 bps
///     clearance fee on it drops 7_966 → 7_956. She keeps that 10; the IF receives it no
///     longer. Σ(ALICE, IF) is unchanged.
///
/// `admin_perp_wallet` (51_003_461) and `market_fee_total` (3_461) are UNCHANGED: every trading
/// fee is still collected in full, only its funding source moved. Positions, CAROL, and every
/// order status are identical. Prior value
/// 0x677500b3559bb22e070c48c9134c3d33c37086b2249f3fb1e88e516a04a8fa04.
/// RE-PIN (A1 isolated funding + `BLOCK_COMMITMENT_VERSION` 15→16): funding settles against
/// `pos.margin` instead of the account-global perp wallet (credit AND charge; the wallet leg is
/// gone, the charge falls straight through to the insurance fund once margin is exhausted).
/// Restores isolated-margin containment and makes funding move the liquidation price, matching
/// Binance's measured behaviour. `liquidate_position` also stops loading/writing the account —
/// funding was its only reason to, so the write was a byte-identical re-store (and a spurious
/// `AccountBalanceChanged`). This is an EXECUTION-RULE change, not a layout change, so the
/// values folded into the delta differ. It CHANGES the business snapshot in exactly two places,
/// which are the SAME two units of value:
///   * `alice_account` 999_059_629 → 999_059_631 (+2) and `insurance_fund` 49_007_956 →
///     49_007_954 (−2). ALICE's Phase-8 funding charge of 400 no longer leaves her wallet; it
///     comes out of her position margin instead, so her pre-liquidation margin is 400 thinner
///     (1_591_318 → 1_590_918) and the 50 bps clearance fee levied on it drops 7_956 → 7_954.
///     Her wallet keeps the 400 it was not charged and gets back 400 less margin at
///     liquidation (net 0), and keeps the 2 the IF no longer collects. Σ(ALICE, IF) is
///     unchanged — a pure 2-unit transfer, no phantom value.
///
/// `bob_account` is UNCHANGED at 500_428_954 even though his +400 funding credit now lands in
/// `pos.margin` rather than the wallet: he ends FLAT (`bob_position == (0, 0, 0)`), and closing
/// a position to zero releases its entire remaining margin to the wallet, so the 400 arrives by
/// a different route and no proportional-release floor strands any of it. CAROL, both other
/// positions, `admin_perp_wallet`, `market_fee_total`, `mark_price`, `funding` and every order
/// status are identical. Prior value
/// 0xa3fcb1f0cccd86eacd6ec33cfae4604a98577cd669c1b2d7ff3997aa0bf6b3ad.
/// RE-PIN (per-user market index, derived-ooIM Phase 0 + `BLOCK_COMMITMENT_VERSION` 16→17): a
/// NEW off-trie namespace ("umkt") records, per user, the set of markets they are active in
/// (non-zero position OR at least one resting order) — the inverse of the per-market position
/// registry, which points the wrong way for the derived-ooIM sum and is blind to markets where
/// a user holds only resting orders. Entering/leaving a market now adds a `umkt` write to the
/// block net delta, so the commitment shifts. Purely ADDITIVE: nothing reads the index yet, no
/// existing blob's layout or value changes, and no execution rule moves — the BusinessSnapshot
/// below is UNCHANGED. (This scenario is single-market, so it exercises enter-on-first-order
/// and leave-on-fully-flat; multi-market ordering and the cap are covered by the dedicated
/// `user_market_index` tests.) Prior value
/// 0xfd17be42969baf09c8e87f990e084ff83180f179c3118e13ed4ccf81e5d92828.
/// RE-PIN (derived open-order margin, Phase 2 + `BLOCK_COMMITMENT_VERSION` 17→18): the
/// open-order margin ESCROW is DELETED. `PerpPosition` loses its six reservation fields
/// ("mr", "mrn", "br", "brn", "sr", "srn"), so every position blob shortens and its later
/// fields shift; and placement, cancel, `setLeverage` and the fill paths no longer move the
/// wallet for a reservation, so the account values folded into the delta differ as well. The
/// requirement is now DERIVED on read (`ooIM = ROUND_UP(max(|N + Bid|, |N − Ask|) / L) −
/// ROUND_UP(|N| / L)`) and subtracted at the admission gate rather than debited.
///
/// The BusinessSnapshot below is **UNCHANGED, field for field**, which is worth stating
/// because it is not the general case — the derived and escrow bases disagree on plenty of
/// book shapes. This scenario avoids all of them: it is single-market, every party ends flat
/// or with a one-sided book at leverage 1, and the ONE order still resting at the end (BOB's
/// tail bid) sits against a FLAT position, where `ooIM = ROUND_UP(Bid / 1)` is exactly the
/// 400_000 the escrow held. So BOB's `availablePerpBalance` arrives at the same 500_428_954
/// by a different route: his wallet is 400_000 higher (never debited) and the derived
/// requirement subtracts exactly that. Likewise every maker fill's opening margin, formerly
/// converted 1:1 out of the placement escrow, is now drawn from the wallet at fill time — the
/// same net movement in two steps instead of two. Prior value
/// 0xb6b78e299b6b96f6dcc0c667c76cc4895ae4cd1932b305fdcda9c0907e998ee0.
/// RE-PIN (Assuming-Price sell side + M1′ maker fills + `BLOCK_COMMITMENT_VERSION` 18→19). No
/// layout change; two execution-rule changes plus a deliberate scenario EXTENSION.
///
/// 1. **`Ask` is priced at each sell's ASSUMING PRICE** `max(ROUND_UP(lastTraded × 1.0015),
///    mark, limit)`, not at its limit price — the vendor Cost formula, measured twice on mainnet
///    (run9's eight admission probes; R10's reported `askNotional / q == limit × 1.0015` for a
///    sell resting below the floor). The buy side is untouched: a LONG order's Assuming Price IS
///    its limit price.
/// 2. **A maker fill the wallet cannot fund now FILLS**, driving `perp_wallet_balance` negative,
///    instead of being cancelled through the K9 channel. Binance leaves an under-covered lien
///    alone and only kills orders at liquidation (measured); "don't fill" was model M2, and both
///    surviving candidates M1/M1′ fill.
/// 3. **NEW Phase 9c**, because the previous snapshot was structurally BLIND to (1): its only
///    surviving order was a BUY against a FLAT position, so no `Ask` term reached it. An oracle
///    bump lifts the mark above the book and CAROL — short QTY — rests a sell inside it.
///
/// BusinessSnapshot, field by field:
///   * `mark_price` 70_000_000_000 → **90_000_000_000**. Phase 9c's bump: `indexPrice = 90·TICK`
///     against a `lastTraded` of `80·TICK`, so `median(price1, price2, contract) = 90·TICK`.
///     Mechanism: the added `updateIndexPrice` call, nothing else.
///   * `carol_account.1` (availablePerpBalance) 4_200_000 → **3_300_000**, i.e. −900_000 = the
///     ooIM of the sell Phase 9c rests. `N = −900_000` at the new mark, `PIM = 900_000`,
///     `Ask = 1 QTY × max(80_120_000_000, 90_000_000_000) = 900_000` ⇒
///     `IM = |−900_000 − 900_000| = 1_800_000` ⇒ `ooIM = 900_000`. **Priced at the order's own
///     $81 limit `Ask` would be 810_000 and `ooIM` 810_000, leaving 3_390_000** — so the 90_000
///     difference between 3_300_000 and 3_390_000 IS the Assuming-Price markup, now pinned in
///     the snapshot rather than only in the dedicated tests.
///   * `carol_ask_status` is NEW: `Open`. The sell rests (it is above the surviving 80-tick bid).
///
/// Everything else is IDENTICAL: all three positions, `alice_account`, `bob_account`,
/// `bob_erc20`, `admin_perp_wallet`, `insurance_fund`, `market_fee_total`, `funding` and every
/// other order status. Phase 9c neither trades nor liquidates (ALICE and BOB are flat; CAROL's
/// equity at the new mark is 700_000 against a 150_000 maintenance requirement), and change (2)
/// is not reachable in this scenario at all — no maker here is ever short of its opening margin,
/// which is why it carries no snapshot movement and is covered by
/// `an_unfundable_maker_fill_still_fills_and_the_silo_is_short_not_the_wallet` plus the
/// conservation leg `an_underfunded_maker_fill_conserves_total_system_value` instead. Prior value
/// 0x2c2ab72998e3f53edf5a6bcb3c7ad552f0543babf6830b8fc87ce707b213e8ac.
/// RE-PIN (M1 maker fills + `BLOCK_COMMITMENT_VERSION` 19→20): an underfunded maker fill now
/// funds its opening leg with `min(opening_value / L, cash at hand)` and lets `pos.margin` be
/// SHORT by the remainder, instead of funding the silo in full and driving
/// `perp_wallet_balance` negative (M1′ → M1; R11 measured M1, `derived-ooim-plan.md` §3a).
///
/// **This scenario's write set is BYTE-IDENTICAL across that change, and the BusinessSnapshot is
/// UNCHANGED, field for field** — for the same reason the previous re-pin gave for its own
/// change (2): no maker here is ever short of its opening margin, so the capped and uncapped
/// branches compute the same `opening_margin` everywhere in it. VERIFIED, not assumed: the whole
/// suite including this pin passed at the OLD value with the new execution rule in place and the
/// version byte still 19. The value below therefore moves for exactly ONE reason — the version
/// byte is hashed into the commitment (`compute_block_commitment`) — and the underlying delta is
/// unmoved. The rule change itself stays covered by
/// `an_unfundable_maker_fill_still_fills_and_the_silo_is_short_not_the_wallet`,
/// `an_underfunded_maker_fill_conserves_total_system_value` and
/// `risk::tests::usdc_custody`. Prior value
/// 0x4a3ed7121a77b1e482a3db667cbe90b0331aa3acd5a350c9508f53e11d33c061.
/// RE-PIN (frozen per-order Assuming Price, R12 + `BLOCK_COMMITMENT_VERSION` 20→21). BOTH a
/// layout and an execution-rule change:
///
/// 1. **`OrderEntry` gains a trailing `assuming_price` ("ap") field**, so every per-user
///    order-list blob grows by one integer. This scenario writes those lists on every
///    place/cancel/fill, so the delta's bytes move on this ground alone.
/// 2. **A resting order's contribution to `Bid`/`Ask` is FROZEN at placement** instead of being
///    re-derived at each read from the CURRENT `T = max(ROUND_UP(lastTraded × 1.0015), mark)`.
///    `total_sell_notional` consequently CARRIES the markup (it was the limit-price baseline
///    before), so the position blob's value changes for any user holding a marked-up resting
///    sell — CAROL's Phase-9c ask is exactly that, at 900_000 rather than 810_000.
///    MEASURED: R12, `misc/binance-flip-and-admission.md` §3.13 — 90 frames, the reported
///    `askNotional` never moved; `H_live` refused by 1939 quanta.
///
/// **The BusinessSnapshot below is UNCHANGED, field for field.** That is not a general property
/// of this change — the frozen and live bases disagree on any book read after a market move —
/// and it holds here for a specific, checkable reason: the scenario has exactly ONE marked-up
/// resting sell (CAROL's Phase-9c ask), it is placed AFTER the last mark/oracle move, and it is
/// read back at that same mark. So the frozen value and the re-derived value coincide: `T` at
/// placement was `max(⌈80·TICK × 1.0015⌉, 90·TICK) = 90·TICK`, dominating its own $81 limit, and
/// `carol_account.1` stays 3_300_000 = 5_000_000 − 800_000 − 900_000 by both routes. Nothing
/// else in the scenario holds a resting sell at all, so `Ask` is 0 for ALICE and BOB either way,
/// and the buy side is definitionally unaffected (a long order's Assuming Price IS its limit
/// price, so `assuming_price == price` on every buy entry ever written). VERIFIED, not assumed:
/// the snapshot assertion passes unchanged, and the value below was captured with the version
/// byte already at 21. Prior value
/// 0x88dc1d5927c36e45585ce99ed8453bf7665a57c89ceb29ee10165fa4ccf41319 (and 0x68a881f4… is the
/// same write set at version byte 20, recorded to separate the two contributions to the move).
/// RE-PIN (out-of-band resting-order expiry + `BLOCK_COMMITMENT_VERSION` 21→22): `updateIndexPrice`
/// now expires the contiguous NEAR-SIDE prefix of levels the moved price band has stranded (asks
/// below the lower edge, bids above the upper edge), capped at `MAX_BAND_EXPIRIES_PER_UPDATE`
/// orders, before the liquidation sweep. `OrderCancelled` also gains a `uint8 reason` so a
/// protocol kill is distinguishable from the owner's own cancel.
///
/// **This scenario's write set is BYTE-IDENTICAL across that change, and the BusinessSnapshot is
/// UNCHANGED, field for field**, for a mechanical reason: the scenario deliberately runs with the
/// band DISABLED (`priceBandBps: 1_000_000`, set when the fill-time band landed — see the 2026-07
/// entry above), and `mark_band_bounds` collapses a disabled band to `lower = 0` / an enormous
/// `upper`, so no level on either side is ever out of band and the new sweep expires nothing here.
/// The two oracle updates it performs (Phase 8's funding epoch and Phase 9c's bump) therefore write
/// exactly what they wrote at 21. The event change cannot reach the snapshot either — every field
/// below is read back through a VIEW call, not from logs.
///
/// VERIFIED, not assumed: the whole suite including this pin passed at the OLD value with the new
/// execution rule in place and the version byte still 21. The value below therefore moves for
/// exactly ONE reason — the version byte is hashed into the commitment — and the underlying delta
/// is unmoved. The rule itself is covered by `risk::tests::band_expiry` (near-side prefix expired,
/// far-side deep orders spared, cap respected + second update finishes, reason codes, frozen
/// `assuming_price` basis, disabled band inert, fill-time band still the backstop). Prior value
/// 0xc84dfc3ed57226c907fbac53e64dfc011e64be2254b092edffb4758a8eef6ba5.
/// RE-PIN (stored `Σ pos.margin` + `BLOCK_COMMITMENT_VERSION` 22→23): `UserAccount` gains a
/// trailing "PM" field (`total_position_margin`), so every stored account blob grows by one
/// integer; and `storage::save_position` now maintains it from the delta of the position write,
/// which adds the ACCOUNT key to the delta of any write that MOVES `pos.margin`. So the commitment
/// moves for THREE mechanical reasons — longer account blobs, extra account keys, and the version
/// byte — none of them an execution rule.
///
/// **BusinessSnapshot UNCHANGED, field for field**, and necessarily so: no field of it reads the
/// new aggregate (`alice/bob/carol_account` decode `getAccount`'s `availableBalance`, which is
/// `totalCrossWalletBalance − totalOpenOrderInitialMargin` and contains no `Σ pos.margin` term;
/// the three `*_position` triples read `getPosition`, which this change does not touch), no
/// existing field of any blob changes value, and nothing conditions on the new field.
/// `totalWalletBalance`, the field the aggregate actually serves, is not pinned by the snapshot —
/// it is pinned against the wide fold by
/// `margin_view::tests::the_event_and_get_account_agree_field_for_field_on_the_same_state` and by
/// the `debug_assertions` cross-check inside `margin_view::index_account_scalars`, which compares
/// the stored aggregate against a fresh walk on every `getAccount` and every published snapshot.
/// Prior value 0xea28799e35db06c71ad95e1517df8e289028b79721bcdb27c183f7d594d72f0e.
/// RE-PIN (per-position cumulative realised PnL + `BLOCK_COMMITMENT_VERSION` 23→24): a LAYOUT
/// change only. `PerpPosition` gains a trailing "cr" field
/// (`cumulative_realized_pnl` = `ACCOUNT_UPDATE.a.P[].cr`), so every stored position blob grows
/// by one integer, and every blob written after a CLOSE carries a different trailing value. So
/// the commitment moves for TWO mechanical reasons — longer position blobs and the version byte
/// — and no execution rule moves with it: nothing reads the field back to make a decision, no
/// balance identity contains it, no gate conditions on it, and the set of keys a write touches
/// is unchanged.
///
/// **BusinessSnapshot UNCHANGED, field for field**, and necessarily so: the three `*_position`
/// triples read `getPosition`, whose five returns do not include the new field; the three
/// `*_account` pairs read spot USDC and `getAccount().availableBalance`
/// (`totalCrossWalletBalance − totalOpenOrderInitialMargin`), neither of which has a `cr` term;
/// and no money is routed differently, so `admin_perp_wallet`, `insurance_fund`,
/// `market_fee_total`, `mark_price`, `funding` and all eleven order statuses are untouched. The
/// field's own values are pinned separately and much more sharply by
/// `trading::tests::position_changed_derived_fields` (the survival test across a close and a
/// reopen, and the Σ-per-fill identity on both sides).
/// Re-pinned when the signed entrypoints were brought onto one replay model. `setLeverageSigned`
/// gained the seen-signature guard it never had, and `cancelOrderSigned` traded terminal-status
/// idempotence for the same explicit burn. The scenario makes one call of each, so the block's
/// net delta gains FOUR entries — a replay marker and a GC bucket per call. That is the whole
/// delta: no existing key changed value, and `BusinessSnapshot` is unchanged, which is the
/// load-bearing check — a replay marker is not money and no balance, position or order status
/// reads it.
///
/// ⚠️ Measured while re-pinning, worth knowing before the next attempt: this hash is
/// INSENSITIVE to write ORDER within a block. Moving a burn from after `place_order_core` to
/// before it and back left the value identical, because #16d hashes the block's NET delta over
/// sorted keys. The assertion message below still says "write-stream commitment"; that wording
/// predates #16d. Only the key SET and the values move this number.
///
/// Guard behaviour itself is pinned by `tests::signed_replay`.
///
/// ── Re-pinned again for the reduce-only groundwork ──────────────────────────────────────
/// A CHAIN change: `BLOCK_COMMITMENT_VERSION` 24 → 26, devnet wiped. Three things moved, none
/// of them behaviour:
///
///   * the VERSION byte is hashed into every block commitment, so the bump alone re-pins this;
///   * `Order` / `OrderEntry` gained `reduce_only`. The codec is POSITIONAL msgpack (field
///     names are not serialised), so an appended field costs its encoded width — one byte for
///     a `false` bool — on EVERY order blob. It is not free, and it is not skippable:
///     `skip_serializing_if` would shift every later field's position and break decoding.
///     Append only; never insert or reorder.
///   * `PerpPosition::reduce_only_qty` was added and then REMOVED again (VERSION 25 → 26). It
///     was meant as the O(1) `Σ reduce-only` term and fast-path gate, but it would have needed
///     maintaining at five sites — rest, cancel, evict, maker fill, liquidation's cancel-all —
///     each one a place to forget, and a stale counter mis-prices admission silently. It is
///     DERIVED from the entry list instead. The list is already warm on every path that needs
///     it (placement loads it to insert; the match flush holds it in the working copy), so the
///     "keeps the predicate off the hot path" argument for storing it was weaker than it
///     looked: the saving was one hashbrown probe, against a whole class of drift.
///   * `placeOrder` / `placeOrderSigned` / `PlaceItem` gained `flags`, widening the signed
///     message to 97 bytes and the batch digest item to 44. Neither is hashed here, but the
///     scenario's signed order re-signs over the new layout.
///
/// `BusinessSnapshot` is unchanged, which is the load-bearing check. reduce-only is now fully
/// enabled, but the golden scenario places no reduce-only order, and the eviction pass exits on
/// its first check when a user holds none — so no order, position, balance or status moved.
///
/// Re-pinned again for the signed-message chain-id binding. Every signed message now starts
/// `prefix || chainId(8)`, so the scenario's signed order and signed cancel produce DIFFERENT
/// SIGNATURE BYTES — and `orderId = keccak256(signature)`, so the order's storage key moves and
/// the seen-signature marker with it. Nothing about the stored FORMAT changed, which is why
/// this needs no `BLOCK_COMMITMENT_VERSION` bump: the key VALUES moved, not the layout.
///
/// `BusinessSnapshot` is unchanged — the ids are opaque to it, only statuses are read.
///
/// Re-pinned again for roadmap 2b, `removePositionMargin` on Binance's withdrawal cap:
/// `BLOCK_COMMITMENT_VERSION` 31 → 32, and that byte is the ONLY thing that moved. Phase 8's
/// withdrawal (250_000 of the 500_000 Phase 7 added) is inside the new cap, so the write set is
/// byte-identical and `BusinessSnapshot` is unchanged — confirmed by running the scenario with the
/// new rule in place against the old pin before bumping the byte.
///
/// Re-pinned again for funding out of EQUITY (`BLOCK_COMMITMENT_VERSION` 32 → 33): a charge may
/// run `pos.margin` negative, closes release margin signed, and ADL settles its winners' funding.
/// Again only the version byte moved — the scenario's one funding charge (400) is far inside
/// ALICE's margin and its ADL winners carry no pending funding, so the write set and
/// `BusinessSnapshot` are byte-identical, confirmed against the old pin before the bump.
///
/// Re-pinned again for sub-accounts (P1, `BLOCK_COMMITMENT_VERSION` 33 → 34): `UserAccount`
/// gains the two trailing family fields ("MA" `master`, "SC" `sub_count`), so every account blob
/// the scenario writes is two bytes longer (`nil` + fixint 0) — a LAYOUT change. The scenario
/// creates no sub-account and makes no internal transfer, so no execution path it takes moved and
/// `BusinessSnapshot` is UNCHANGED, field for field (asserted below).
///
/// Re-pinned again for `cancelAlgoOrderSigned` burning its signature (34 → 35). The scenario
/// sends no signed algo cancel, so only the version byte moved.
///
/// Re-pinned again for the self-trade-prevention series (35 → 36), first for the per-market
/// open-order cap (200, book + conditional). The scenario never holds more than a handful of
/// orders per user, so no placement in it is refused, its write set is byte-identical and
/// `BusinessSnapshot` is UNCHANGED — VERIFIED, not assumed: with the cap in place and the
/// version byte still 35 the scenario reproduced the old pin 0x2ffa38ae…b88d exactly. (The
/// direct `placeAlgoOrder` now persists its nonce bump AFTER the placement instead of before;
/// same key, same value, and #16d hashes the net delta, so that reorder is invisible here.)
///
/// Re-pinned again within 36 for the STP encoding: `Order` and `OrderEntry` each gain a trailing
/// "stp" field (positional msgpack, appended), so every order blob and every per-user entry
/// list the scenario writes is one byte per order longer — CAROL's untriggered Phase-9d stop
/// included. A LAYOUT change only: no order in the scenario sets STP bits, matching does not
/// read the field yet, and `BusinessSnapshot` is UNCHANGED. VERIFIED: with both fields
/// `#[serde(skip)]` the scenario reproduced the previous pin 0x064ca8b4…8912 exactly.
///
/// Re-pinned for trading fees in ppm (`BLOCK_COMMITMENT_VERSION` 36 → 37). The scenario's
/// `setUserFeeRates` calls move from 2/5 bps to the same rates in ppm, 200/500, so every fee it
/// charges is the same integer (`floor(n·200/1e6) == floor(n·2/1e4)`). What moved is the bytes:
/// the account blobs and order-entry snapshots now carry 200/500 where they carried 2/5, and a
/// msgpack integer above 127 is wider than a fixint. Plus the version byte. The
/// `BusinessSnapshot` is UNCHANGED, field for field (asserted below).
///
/// Re-pinned for the conditional-order series (`BLOCK_COMMITMENT_VERSION` 37 → 38), first for
/// the `fire()` capacity fix: a fired order that fills immediately is bounded by `|position|`
/// alone. The scenario's one conditional (CAROL's Phase-9d stop) never fires, so only the
/// version byte moved — VERIFIED: with the fix in place and the byte still 37 the scenario
/// reproduced the previous pin 0x525fea15…33ee. `BusinessSnapshot` UNCHANGED.
///
/// Re-pinned again within 38 for closePosition parity: `PerpPosition` gains a trailing
/// "g" (`generation`) and `Order` a trailing "pg" (`position_generation`), positional msgpack,
/// appended — so every position blob and every order blob the scenario writes is one byte
/// longer (a fixint: 0 on every order, since CAROL's stop is a reduce-only quantity order and
/// bound to nothing; on a position, how many times it has gone flat or flipped). A LAYOUT
/// change only: the new placement rules (reduceOnly + closePosition refused, "would
/// immediately trigger" refused, closePosition bound to an open position) refuse nothing the
/// scenario sends — CAROL's BUY stop sits above the mark — and its stop never fires.
/// `BusinessSnapshot` UNCHANGED. VERIFIED: with both fields `#[serde(skip)]` the scenario
/// reproduced the previous pin 0x720411d8…e3e0.
///
/// Re-pinned again within 38 for the trailing stop: `Order` gains a trailing "tb"
/// (`trail_bps`, u16, 0 = not trailing), positional msgpack, appended — so every order blob the
/// scenario writes is one byte longer (a fixint 0: it places no trailing stop). A LAYOUT change
/// only. The trailing structure itself writes nothing here: each oracle tick reads its (absent)
/// meta key and stops. `BusinessSnapshot` UNCHANGED. VERIFIED: with the field `#[serde(skip)]`
/// the scenario reproduced the previous pin 0xa72ce19d…d940.
///
/// Re-pinned for the cross-margin series (`BLOCK_COMMITMENT_VERSION` 38 → 39), first for P0,
/// the explicit maintenance-rate column: `MarginTier` gains a trailing `maint_margin_rate_ppm`
/// (u32, 0 = derived `1/(2L)`), positional msgpack, appended — so every market blob the
/// scenario writes is one byte longer per tier row (a fixint 0: it installs no table, every
/// market keeps the default `{0, 3x, derived}`). A LAYOUT change only; with a zero column the
/// maintenance requirement is the same expression byte for byte. `BusinessSnapshot` UNCHANGED.
/// VERIFIED: with the field `#[serde(skip)]` the scenario reproduced the previous 38 pin
/// 0x37a18141…79de.
///
/// Re-pinned again within 39 for P1, the margin type: `PerpPosition` gains a trailing "mt"
/// (`margin_type`, u8, 0 = CROSSED — Binance's default), positional msgpack, appended — so
/// every position blob the scenario writes is one byte longer (a fixint 0: nobody calls
/// `setMarginType`). A LAYOUT change only. Cross margin is gated off
/// (`CROSS_MARGIN_ENABLED = false`), so every position is ISOLATED in force and no rule reads
/// the byte; `BusinessSnapshot` UNCHANGED. VERIFIED: with the field `#[serde(skip)]` the
/// scenario reproduced the previous 39 (P0) pin 0xafd82645…7b48.
///
/// Re-pinned again within 39 for P2 (cross accounting), three appended fields: `Market` gains
/// "fx" (`cumulative_funding_index`, i128 — the mirror of `FundingState`'s index; 0 here, the
/// scenario crosses no funding epoch) and `UserAccount` gains "LF" (`liq_flags`, u8, never set
/// before P3) and "OP" (`open_position_count`, u8, maintained by `save_position` on every
/// open/close) — so every market and account blob the scenario writes grows. A LAYOUT change
/// only: the count rides the account write an open or close already makes, and every position
/// is ISOLATED in force. `BusinessSnapshot` UNCHANGED. VERIFIED: with all three fields
/// `#[serde(skip)]` the scenario reproduced the previous 39 (P1) pin 0x7deaa026…6637.
///
/// Re-pinned again within 39 for P3 (cross liquidation, owner decision Q3 / J5 / R7): the
/// liquidation clearance fee is a rate on the NOTIONAL the liquidation closed (valued at the
/// mark), no longer on the silo margin it consumed, and an isolated liquidation's fee is capped
/// at the close credit its close returned to the wallet. The INTENDED isolated business change,
/// and it moves exactly two `BusinessSnapshot` fields, by the same 6_046: ALICE's liquidation
/// closes 0.04 @ the $70 mark = 2_800_000 of notional, so her 50 bps fee is 14_000 (it was
/// 7_954 = 50 bps of her 1_590_918 silo; the J5 cap, her ~1.4M close credit, does not bind) —
/// `alice_account.1` 999_059_631 → 999_053_585 and `insurance_fund` 49_007_954 → 49_014_000.
/// Nothing else moves. Prior value 0x8c500d81725e79c199fc06ab19800c60c25a914672399c09d6110ef2d5c2a62c
/// (39, P2 cross accounting).
///
/// Re-pinned again within 39 for the P3 FLIP (R8, `CROSS_MARGIN_ENABLED = true`): a position
/// that never chose is CROSSED in force, so this scenario — which pins ISOLATED margin — has
/// ALICE, BOB and CAROL call `setMarginType(ISOLATED)` on the flat market first (Phase 3b).
/// Their position records store 1 where they stored 0; nothing else in the write set moves.
/// VERIFIED: with the gate forced off and the three switches skipped, the scenario reproduced
/// the previous pin 0xaea562b9…1e61. `BusinessSnapshot` UNCHANGED — the flip moves none of it.
/// Prior value 0xaea562b976f3f646a94f8cfbf468c1caf8e7c7465e30f351f3fb8ec719c91e61 (39, P3
/// clearance fee).
///
/// Prior values 0x7deaa0263925b95e203cf08b43fbf93b34f7af725d19933d6e72510cd6c86637 (39, P1
/// margin type), 0xafd826451ed6c8e356b03def92beb67c4e2431333141b39bfc21aeef4f057b48 (39, P0
/// maintenance-rate column), 0x37a18141947ba3bb4fe923fe0e05a07d4dc0a8d1987096250991ed7d0f3f79de (38,
/// trailing stop), 0xa72ce19d4e1c033989c620e39e9a7e09d6b483aee1c38cfe0d9e08770898d940 (38,
/// closePosition parity), 0x720411d896e56b98c027a692b9cb8afba35c93accce39ccbef86a1a59f93e3e0 (38,
/// fire() capacity fix), 0x525fea15e0541c3a4441899ac1d7862a0fc5d2cb3dae2a933a6b3b5594d933ee (37,
/// ppm fees + maker rebate),
/// 0xc1379f42cc51db429ce10ab455e6daeb918a81bca8df7d210ae584768e631988 (36, STP),
/// 0x064ca8b4d053dc1def05f9991d75afd9dc52440890525c0a2efbc018382f8912,
/// 0x2ffa38aefbdfa68307eb6869b95cb48d9828f407dce2924f2abd8ef5c5c0b88d,
/// 0x8362c452406bbd6f048358985803964c672ad20444e3f362327114a403b7ebb8,
/// 0x913b6760140fa04bdd42be6a6b63908f5d1d16430b96a1c8ce1b84b7612540ce,
/// 0x6ef01990b3e35d7e6cec4df47218bbdf3b857b7355e36c42d9318b142a742052,
/// 0x38ca0434996ba2e636a05ff1c69c46c9ff93d343e4cde19e8223968a6a03d733,
/// 0x73e4a2f0cebdd3752b677a465759c090637673c4077755dc2d4037563588e7fe,
/// 0x345e5092f80e665e92fdb64d0cf72c27d466d9481a8784104b042b854e985875,
/// 0xe533ca6b378b7819b0f45def0fe712a6c8449805cb09888d0466cfd8b33e0e20,
/// 0xd1c1019a950f855852ea7ee35373c88a48cba0679de161c5e5f0ae5787c1c765.
pub(crate) const GOLDEN_COMMITMENT: B256 =
    b256!("0xfdf928b54c849a832e948e32d20b951490ebdc33c54c38254a13f10351a147d4");

/// Business end-state read back through view calls after the scenario.
/// Pins semantics independently of the commitment hash construction.
///
/// It does NOT pin the positive-maker fee on a PARTIALLY filled order. That fee is the
/// whole-order difference (`entry_fill_plan`), and none of this scenario's positive-rate
/// partial fills truncates differently under the difference and under an independent per-fill
/// floor. A mutant that switches to the per-fill floor leaves both the commitment and this
/// snapshot unchanged. Adding such a fill here would move the snapshot, so the path is pinned
/// instead in `fee_spec_tests`, by
/// `a_positive_maker_pays_the_whole_order_difference_across_partial_fills` (balances) and
/// `a_partially_filled_order_is_rebated_per_fill_on_its_own_floor` (`Trade.makerFee`).
#[derive(Debug, PartialEq, Eq)]
struct BusinessSnapshot {
    /// (amount, vQuoteBalance, margin)
    alice_position: (i64, i64, i64),
    bob_position: (i64, i64, i64),
    carol_position: (i64, i64, i64),
    /// (spot USDC balance, `getAccount().availableBalance`)
    ///
    /// The second element widened `u64` → `i64` when `getAccount`'s clamped
    /// `availablePerpBalance` was replaced by the signed `availableBalance`. Every VALUE this
    /// scenario pins is unchanged — the quantity is the same `perp_wallet_balance - SUM ooIM`,
    /// and none of these accounts is under water at the end of the scenario, so nothing was
    /// being floored away for the clamp to hide.
    alice_account: (U256, i64),
    bob_account: (U256, i64),
    carol_account: (U256, i64),
    bob_erc20: U256,
    /// Trading-fee sink (taker+maker fees credit the admin's perp wallet).
    admin_perp_wallet: i64,
    insurance_fund: u64,
    market_fee_total: u64,
    mark_price: u64,
    /// (fundingRate, nextFundingTime)
    funding: (i64, u64),
    signed_buy_status: u8,
    gtc_cancelled_status: u8,
    ioc_status: u8,
    /// Market-order remainder against an empty book (market path → Expired).
    mkt_expired_status: u8,
    /// PostOnly ask that rested, then was cancelled (sell-side cancel paths).
    po_ask_cancelled_status: u8,
    /// ALICE's resting bid cancelled by liquidate()'s cancel-all (buy side).
    liq_cancelled_bid_status: u8,
    /// ALICE's resting ask cancelled by liquidate()'s cancel-all (sell side).
    liq_cancelled_ask_status: u8,
    bob_bid_status: u8,
    /// BOB's same-price tail bid surviving the sell-side matcher early-exit.
    bob_tail_bid_status: u8,
    carol_close_status: u8,
    /// CAROL's resting ask against her SHORT — the scenario's only sell-side open-order
    /// requirement, and the only place the Assuming-Price markup reaches the snapshot.
    carol_ask_status: u8,
}

fn expected_snapshot() -> BusinessSnapshot {
    BusinessSnapshot {
        // ALICE fully closed by the liquidation round-trip.
        alice_position: (0, 0, 0),
        // ⚠️ BOB IS NO LONGER FLAT, and the reason is the whole point of the residual rework.
        // ALICE's liquidation residual used to be closed at the mark against NO COUNTERPARTY —
        // it simply evaporated. It is now a real ADL trade, and BOB is the opposite side: one
        // `Adl` event, loser ALICE, adlUser BOB, 0.01 @ the $70 mark (solvent residual, so the
        // mark and not the bankruptcy price). That consumed 0.01 of BOB's short at that point
        // in the scenario, so CAROL's later taker sell into his resting bid leaves him LONG
        // 0.01 where it used to leave him flat.
        //
        // He is ABOVE MAINTENANCE here — an ordinary healthy open position, NOT a deferred
        // liquidation. Σ amount is still conserved; ALICE's exposure MOVED to BOB instead of
        // vanishing, which is exactly what `liquidation_conserves_sigma_amount` asserts.
        bob_position: (1_000_000, -800_000, 399_840),
        // CAROL short QTY @ $80 at default leverage 1 (full-notional margin).
        // Her position is untouched by the leverage retune, but the deeper crash
        // leaves her holding 100_000 of UNREALISED gain (1e6 base × $10 of extra
        // downside). That is why the account-side total below is exactly 100_000
        // lower than the pre-retune snapshot — the value moved into her uPnL, which
        // these account fields do not carry. Conservation holds.
        carol_position: (-1_000_000, 800_000, 800_000),
        // ALICE. Two independent drivers changed this vs the pre-cap snapshot
        // (999_162_305), and nothing else did:
        //   1. leverage 5 → 3 (max-leverage cap): every fill posts thicker margin,
        //      so her pre-liquidation margin — and thus the 50 bps clearance fee —
        //      grew (1_056_000 → 1_593_200, fee 5_280 → 7_966).
        //   2. crash $80 → $70: the residual QTY settles at a worse mark, so the
        //      liquidation returns less.
        // The liquidation close taker fee is still WAIVED (fix B) — no −1_200 here.
        // The old term-by-term breakdown is superseded: it was pinned to the 5x
        // margins and the $80 settle, and every term moved.
        //   3. fee_reserved removal: her opening taker fees are charged to the
        //      position margin instead of the wallet, so the margin the clearance
        //      fee is levied on is 2_000 thinner and that fee is 10 smaller — the
        //      10 stays with her (and the IF below is 10 lower). Nothing else on
        //      her side moves: the 2_000 she does not pay from the wallet is
        //      exactly the 2_000 less that her margin returns at liquidation.
        //   4. A1 isolated funding: same shape, one step smaller. Her 400 funding
        //      charge is taken from the position margin instead of the wallet, so
        //      the pre-liquidation margin is a further 400 thinner (1_591_318 →
        //      1_590_918) and the clearance fee drops 7_956 → 7_954. The 400 she
        //      does not pay from the wallet is exactly the 400 less her margin
        //      returns, so the only net movement is the +2 she keeps (= the 2 the
        //      IF below no longer collects).
        //   5. cross P3 (Q3 / J5): the clearance fee is 50 bps of the NOTIONAL closed
        //      (0.04 @ $70 = 2_800_000 → 14_000), no longer of the silo margin (7_954):
        //      6_046 more leaves her wallet for the IF below (999_059_631 → 999_053_585).
        alice_account: (U256::from(500_000_000u64), 999_053_585),
        // BOB perp = 1e9 + 830_000 short PnL (622_500 on the 3-QTY
        //   liquidation leg + 207_500 on the QTY closed via CAROL) + 400
        //   funding credit − 1_446 maker fees − 400_000 still reserved for
        //   the resting tail bid (margin only: the 160 fee that used to be
        //   escrowed alongside it is no longer withheld at placement)
        //   − 500_000_000 transferFromPerp.
        // A1 note: the 400 funding credit is now paid into his POSITION margin, not
        //   straight into the wallet. He ends flat, and closing to zero releases the
        //   whole remaining margin, so the 400 still lands here — unchanged total,
        //   different route.
        // Lower than before (500_428_954) because he ends HOLDING a position instead of flat:
        // its margin is still locked and its PnL still unrealised. Nothing is lost — available
        // is not balance.
        bob_account: (U256::from(500_000_000u64), 500_129_114),
        // CAROL perp = 5_000_000 funded − 800_000 short opening margin − 900_000 of DERIVED
        // ooIM for the Phase-9c resting sell. That 900_000 is the sell priced at the ASSUMING
        // PRICE (the $90 mark, which dominates both its own $81 limit and the
        // ROUND_UP(80·TICK × 1.0015) = $80.12 last-price term); at the limit price it would be
        // 810_000 and this field would read 3_390_000. Nothing is debited — ooIM is subtracted
        // on read, so her wallet is still 4_200_000.
        carol_account: (U256::from(5_000_000u64), 3_300_000),
        // 2e9 seed − 1.5e9 deposit + 0.5e9 withdraw.
        bob_erc20: U256::from(1_000_000_000u64),
        // 100M funding − 50M IF deposit + 1M IF withdraw + 3_461 fees
        //   (liquidation close taker fee waived — fix B).
        admin_perp_wallet: 51_003_461,
        // 50M deposit − 1M withdraw + 14_000 clearance fee: 50 bps of the 2_800_000
        //   NOTIONAL ALICE's liquidation closed (0.04 @ the $70 mark — cross P3, Q3). It
        //   was 7_954 = 50 bps of her 1_590_918 pre-liquidation silo margin until the fee
        //   moved to the notional basis; the 6_046 difference is exactly what ALICE's
        //   wallet above gives up. Its J5 cap (the close credit her close returned to the
        //   wallet) does not bind.
        insurance_fund: 49_014_000,
        // ALICE takers 2_015 + BOB maker 806 + 480 + 160 (CAROL's taker fee
        //   is 0 bps; the liquidation close taker fee is waived — fix B).
        market_fee_total: 3_461,
        // $90: Phase 9c's oracle bump lifts the mark ABOVE the book so the `Mark` branch of the
        // Assuming Price is reachable (a $1 tick cannot express the 15 bps `Last × 1.0015` gap).
        mark_price: 90_000_000_000,
        funding: (100, 7_215), // rate = interest-rate clamp; next epoch ts
        // delete-on-terminal (commit-only #23): every terminal order (Filled/Cancelled/Expired)
        // is removed from the map → getOrder reverts → DELETED sentinel. Only the still-resting
        // tail bid remains queryable (Open). Business outcomes are pinned by the position /
        // account / fee fields above, not these status bytes.
        signed_buy_status: DELETED,
        gtc_cancelled_status: DELETED,
        ioc_status: DELETED,
        mkt_expired_status: DELETED,
        po_ask_cancelled_status: DELETED,
        liq_cancelled_bid_status: DELETED,
        liq_cancelled_ask_status: DELETED,
        bob_bid_status: DELETED,
        bob_tail_bid_status: OrderStatus::Open as u8,
        carol_close_status: DELETED,
        carol_ask_status: OrderStatus::Open as u8,
    }
}

// ── Context & call plumbing ────────────────────────────────────────────

/// Consensus-visible commitment anchor slot under 0x1003, derived locally
/// (`keccak256(b"cmit")`) and NOT via `storage::keys::commitment_slot()`,
/// so a refactor that consistently changes the production slot derivation
/// (e.g. a typo'd precomputed constant) cannot pass self-consistently:
/// the explicit slot-pin assert at the top of `run_golden_scenario` fails
/// with a clear message instead.
fn golden_commitment_slot() -> B256 {
    // Independent re-derivation of the packed anchor slot (#12): "cmit" ++ 28 zero.
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(b"cmit");
    B256::new(b)
}

/// Standard OZ ERC-20 `_balances[user]` slot (mapping at slot 0), derived
/// locally for the same anti-tautology reason as `golden_commitment_slot`.
fn golden_erc20_balance_slot(user: Address) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(user.as_slice());
    keccak256(buf)
}

/// Fresh context with ERC-20 USDC seeded for ALICE / BOB / CAROL / ADMIN so
/// the full deposit → transferToPerp pipeline can run through the entry point.
fn golden_ctx() -> TestCtx {
    let mut db = InMemoryDB::default();
    for (user, amount) in [
        (ALICE, 2_000_000_000u64), // $2,000
        (BOB, 2_000_000_000),
        (CAROL, 10_000_000),  // $10 — only used for the post-liquidation step
        (ADMIN, 500_000_000), // $500
    ] {
        db.insert_account_storage(
            USDC_ADDRESS,
            golden_erc20_balance_slot(user).into(),
            U256::from(amount),
        )
        .unwrap();
    }
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [
        USDC_ADDRESS,
        PERP_DEX_ADDRESS,
        ALICE,
        BOB,
        CAROL,
        ADMIN,
        ORACLE,
        MANAGER,
        NEW_ADMIN,
    ] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    // Fixed block timestamp for full determinism (signed-order recvWindow
    // checks and mid-price samples both read it).
    ctx.block.timestamp = U256::from(1);
    ctx
}

fn read_commitment(ctx: &mut TestCtx) -> U256 {
    ctx.journal_mut()
        .sload(PERP_DEX_ADDRESS, golden_commitment_slot().into())
        .unwrap()
        .data
}

fn decode_revert(bytes: &[u8]) -> String {
    if bytes.len() >= 68 && bytes[..4] == [0x08, 0xc3, 0x79, 0xa0] {
        let len = U256::from_be_slice(&bytes[36..68]).to::<usize>();
        String::from_utf8_lossy(&bytes[68..68 + len]).into_owned()
    } else {
        format!("0x{}", primitives::hex::encode(bytes))
    }
}

/// Mutating call through the full entry point; panics with the decoded
/// revert reason on failure.
fn dex_call(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> Bytes {
    let out = run_perp_dex_call(input, 10_000_000, caller, U256::ZERO, false, ctx)
        .expect("perp dex call must not hard-fail");
    assert!(
        !out.reverted,
        "dex call reverted: {}",
        decode_revert(&out.bytes)
    );
    out.bytes
}

/// A call whose revert is the point. Mirrors the EVM frame: the call runs
/// under a checkpoint that is rolled back on revert, and the commitment
/// must come out unchanged.
fn dex_call_expect_revert(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) {
    let c_before = read_commitment(ctx);
    let cp = ctx.journal_mut().checkpoint();
    let out = run_perp_dex_call(input, 10_000_000, caller, U256::ZERO, false, ctx)
        .expect("perp dex call must not hard-fail");
    assert!(
        out.reverted,
        "expected revert containing {expect:?}, call succeeded"
    );
    let reason = decode_revert(&out.bytes);
    assert!(
        reason.contains(expect),
        "revert reason {reason:?} does not contain {expect:?}"
    );
    ctx.journal_mut().checkpoint_revert(cp);
    assert_eq!(
        read_commitment(ctx),
        c_before,
        "reverted call must leave the commitment unchanged"
    );
}

/// Static (view) call through the entry point.
fn dex_view(ctx: &mut TestCtx, input: &[u8]) -> Bytes {
    let out = run_perp_dex_call(input, 10_000_000, CAROL, U256::ZERO, true, ctx)
        .expect("view call must not hard-fail");
    assert!(
        !out.reverted,
        "view call reverted: {}",
        decode_revert(&out.bytes)
    );
    out.bytes
}

fn g_place(
    ctx: &mut TestCtx,
    caller: Address,
    side: u8,
    price: u64,
    qty: u64,
    order_type: u8,
    tif: u8,
) -> [u8; 32] {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
                flags: 0,
    }
    .abi_encode();
    let ret = dex_call(ctx, caller, &input);
    ret[..32].try_into().unwrap()
}

/// Sentinel for a terminal order under delete-on-terminal (commit-only #23): a
/// Filled/Cancelled/Expired order is removed from the map, so `getOrder` reverts "not found".
/// The scenario asserts this sentinel where it used to assert the terminal status — the actual
/// business outcome (fill/cancel) is pinned by the position/account/fee/event assertions, not
/// by a queryable status byte.
const DELETED: u8 = 0xFF;

fn order_status(ctx: &mut TestCtx, id: [u8; 32]) -> u8 {
    let out = run_perp_dex_call(
        &getOrderCall {
            orderId: id.into(),
            marketId: MARKET_ID,
        }
        .abi_encode(),
        10_000_000,
        CAROL,
        U256::ZERO,
        true,
        ctx,
    )
    .expect("view call must not hard-fail");
    if out.reverted {
        return DELETED; // delete-on-terminal: terminal order was removed from the map
    }
    getOrderCall::abi_decode_returns(&out.bytes).unwrap().status
}

// ── ed25519 signed-call calldata (fixed key seed, fixed timestamps) ────

const SIGNED_TS: u64 = 1; // == block timestamp
const SIGNED_RECV: u64 = 60;

fn signed_place_input(
    sk: &SigningKey,
    side: u8,
    price: u64,
    qty: u64,
    order_type: u8,
    tif: u8,
    client_id: [u8; 16],
) -> Vec<u8> {
    let mut msg = [0u8; 105];
    let mut c =
        crate::account::signing::write_signed_header(&mut msg, b"perpdex_v1_order", TEST_CHAIN_ID);
    c = crate::account::signing::put(&mut msg, c, ALICE.as_slice());
    c = crate::account::signing::put(&mut msg, c, &MARKET_ID.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &[side]);
    c = crate::account::signing::put(&mut msg, c, &price.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &qty.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &[order_type, tif]);
    c = crate::account::signing::put(&mut msg, c, &client_id);
    c = crate::account::signing::put(&mut msg, c, &SIGNED_TS.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &[0, 0]);
    debug_assert_eq!(c, msg.len());
    let sig = sk.sign(&msg);
    placeOrderSignedCall {
        account: ALICE,
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes(client_id),
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 0,
        signature: sig.to_bytes().to_vec().into(),
                flags: 0,
    }
    .abi_encode()
}

fn signed_cancel_input(sk: &SigningKey, order_id: [u8; 32]) -> Vec<u8> {
    // Canonical 94-byte message, layout from run_cancel_order_signed.
    let mut msg = [0u8; 102];
    let mut c =
        crate::account::signing::write_signed_header(&mut msg, b"perpdex_v1_cancel", TEST_CHAIN_ID);
    c = crate::account::signing::put(&mut msg, c, &ALICE.as_slice());
    c = crate::account::signing::put(&mut msg, c, &order_id);
    c = crate::account::signing::put(&mut msg, c, &MARKET_ID.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &SIGNED_TS.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &[0]);
    debug_assert_eq!(c, msg.len());
    let sig = sk.sign(&msg);
    cancelOrderSignedCall {
        account: ALICE,
        orderId: order_id.into(),
        marketId: MARKET_ID,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 0,
        signature: sig.to_bytes().to_vec().into(),
    }
    .abi_encode()
}

fn signed_leverage_input(sk: &SigningKey, leverage: u64) -> Vec<u8> {
    // Canonical 72-byte message, layout from run_set_leverage_signed.
    let mut msg = [0u8; 80];
    let mut c = crate::account::signing::write_signed_header(
        &mut msg,
        b"perpdex_v1_leverage",
        TEST_CHAIN_ID,
    );
    c = crate::account::signing::put(&mut msg, c, &ALICE.as_slice());
    c = crate::account::signing::put(&mut msg, c, &MARKET_ID.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &leverage.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &SIGNED_TS.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &[0]);
    debug_assert_eq!(c, msg.len());
    let sig = sk.sign(&msg);
    setLeverageSignedCall {
        account: ALICE,
        marketId: MARKET_ID,
        leverage,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 0,
        signature: sig.to_bytes().to_vec().into(),
    }
    .abi_encode()
}

// ── The scenario ───────────────────────────────────────────────────────

/// Plays the fixed script on a fresh context and returns the final
/// commitment-slot value plus the business end-state.
fn run_golden_scenario() -> (B256, BusinessSnapshot) {
    // Pin the consensus-visible slot LOCATIONS against the production
    // derivations: if either derivation ever changes, fail loudly here
    // rather than tautologically reading/writing a silently moved slot.
    assert_eq!(
        golden_commitment_slot(),
        storage_keys::commitment_slot(),
        "commitment anchor slot moved"
    );
    assert_eq!(
        golden_erc20_balance_slot(ALICE),
        storage_keys::erc20_balance_slot(ALICE),
        "erc20 balance slot derivation moved"
    );

    let mut ctx = golden_ctx();
    let sk = SigningKey::from_bytes(&[7u8; 32]);

    // Phase 0 — roles (admin / oracle / market-manager blobs).
    dex_call(
        &mut ctx,
        ADMIN,
        &initAdminCall { admin: ADMIN }.abi_encode(),
    );
    dex_call_expect_revert(
        &mut ctx,
        ALICE,
        &initAdminCall { admin: ALICE }.abi_encode(),
        "already initialised",
    );
    dex_call(
        &mut ctx,
        ADMIN,
        &setOracleAddressCall { oracle: ORACLE }.abi_encode(),
    );
    dex_call(
        &mut ctx,
        ADMIN,
        &setMarketManagerAddressCall { manager: MANAGER }.abi_encode(),
    );

    // Phase 1 — market via the manager role, then an admin updateMarket.
    dex_call(
        &mut ctx,
        MANAGER,
        &addMarketCall {
            marketId: MARKET_ID,
            baseDecimals: 8,
            priceDecimals: 9,
            tickSize: TICK,
            stepSize: QTY,
            minQuantity: QTY,
            maxQuantity: QTY * 1_000,
            maxPrice: PRICE * 10,
            priceUpdateInterval: 15,
            fundingInterval: 3_600,
            interestRate: 100,
            liquidationFeeRateBps: 50,
            initialMarkPrice: PRICE,
            // Disabled band (>> this scenario's max price = 10x mark): the golden
            // scenario is the matching/settlement/commitment regression, not a band
            // test. Band enforcement is exercised by dedicated tests below.
            priceBandBps: 1_000_000,
        }
        .abi_encode(),
    );
    dex_call(
        &mut ctx,
        ADMIN,
        &updateMarketCall {
            marketId: MARKET_ID,
            tickSize: TICK,
            stepSize: QTY,
            minQuantity: QTY,
            maxQuantity: QTY * 2_000,
            maxPrice: PRICE * 10,
            priceUpdateInterval: 15,
            active: true,
            fundingInterval: 3_600,
            interestRate: 100,
            liquidationFeeRateBps: 50,
            priceBandBps: 1_000_000, // keep the golden band disabled (see addMarket above)
        }
        .abi_encode(),
    );

    // Nonzero maker+taker fees for both traders (fee-sink paths need them).
    for user in [ALICE, BOB] {
        dex_call(
            &mut ctx,
            ADMIN,
            &setUserFeeRatesCall {
                user,
                makerFeePpm: 200,
                takerFeePpm: 500,
            }
            .abi_encode(),
        );
    }
    dex_call_expect_revert(
        &mut ctx,
        BOB,
        &setUserFeeRatesCall {
            user: BOB,
            makerFeePpm: 0,
            takerFeePpm: 0,
        }
        .abi_encode(),
        "not admin",
    );

    // Phase 2 — oracle bootstrap: mark price must exist before trading.
    dex_call(
        &mut ctx,
        ORACLE,
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: PRICE,
            timestamp: 15,
        }
        .abi_encode(),
    );

    // Phase 3 — fund accounts: ERC-20 → spot (deposit) → perp wallet.
    for user in [ALICE, BOB] {
        dex_call(
            &mut ctx,
            user,
            &depositCall {
                amount: U256::from(1_500_000_000u64),
            }
            .abi_encode(),
        );
        dex_call(
            &mut ctx,
            user,
            &transferToPerpCall {
                amount: 1_000_000_000,
            }
            .abi_encode(),
        );
    }
    dex_call(
        &mut ctx,
        ADMIN,
        &depositCall {
            amount: U256::from(200_000_000u64),
        }
        .abi_encode(),
    );
    dex_call(
        &mut ctx,
        ADMIN,
        &transferToPerpCall {
            amount: 100_000_000,
        }
        .abi_encode(),
    );

    // Phase 3b — margin type. This scenario pins ISOLATED margin: since the cross flip a
    // never-set position is CROSSED (Binance's default), so every trader chooses ISOLATED on
    // the flat, order-free market first — the switch a real user makes, with its
    // `MarginTypeChanged`. The cross scenario is `cross_golden`.
    for user in [ALICE, BOB, CAROL] {
        dex_call(
            &mut ctx,
            user,
            &crate::interface::IPerpDex::setMarginTypeCall {
                marketId: MARKET_ID,
                marginType: crate::types::MarginType::Isolated.code(),
            }
            .abi_encode(),
        );
    }

    // Phase 4 — leverage.
    dex_call(
        &mut ctx,
        ALICE,
        &setLeverageCall {
            marketId: MARKET_ID,
            leverage: 3,
        }
        .abi_encode(),
    );
    dex_call(
        &mut ctx,
        BOB,
        &setLeverageCall {
            marketId: MARKET_ID,
            leverage: 2,
        }
        .abi_encode(),
    );

    // Phase 5 — signed path: register fixed-seed key, signed resting buy
    // (relayed by CAROL — caller is irrelevant on the signed path).
    dex_call(
        &mut ctx,
        ALICE,
        &registerApiKeyCall {
            keyId: 0,
            pubkey: FixedBytes(sk.verifying_key().to_bytes()),
            expiry: 0,
        }
        .abi_encode(),
    );
    let signed_buy: [u8; 32] = {
        let input = signed_place_input(&sk, 0, PRICE - 5 * TICK, QTY, 0, 0, [0xA1; 16]);
        let ret = dex_call(&mut ctx, CAROL, &input);
        ret[..32].try_into().unwrap()
    };

    // While the signed buy rests and the position is still flat, retune
    // leverage both ways so rebalance_order_margin_for_leverage runs its
    // debit branch (3→2 grows the resting-order reserve; relayed by CAROL
    // through the signed path) and its credit branch (2→3 shrinks it back).
    dex_call(&mut ctx, CAROL, &signed_leverage_input(&sk, 2));
    dex_call(
        &mut ctx,
        ALICE,
        &setLeverageCall {
            marketId: MARKET_ID,
            leverage: 3,
        }
        .abi_encode(),
    );

    // View stretch #1 — pure reads must not fold the commitment.
    let c_views = read_commitment(&mut ctx);
    dex_view(
        &mut ctx,
        &getMarkPriceCall {
            marketId: MARKET_ID,
        }
        .abi_encode(),
    );
    dex_view(&mut ctx, &getAccountCall { user: ALICE }.abi_encode());
    dex_view(
        &mut ctx,
        &getOpenOrdersCall {
            user: ALICE,
            marketId: MARKET_ID,
        }
        .abi_encode(),
    );
    dex_view(&mut ctx, &getApiKeysCall { user: ALICE }.abi_encode());
    dex_view(
        &mut ctx,
        &getIndexPriceCall {
            marketId: MARKET_ID,
        }
        .abi_encode(),
    );
    assert_eq!(
        read_commitment(&mut ctx),
        c_views,
        "view calls must not fold the commitment"
    );

    // Phase 6 — direct trading.
    // BOB makes three ask levels, with TWO same-price orders at L1; ALICE
    // takes L1's head with a user-facing MARKET order (the same-price tail
    // must survive via the buy-side matcher early-exit), then sweeps the
    // rest (level clearing + price-list rewrite + best-ask refresh).
    let bob_l1a = g_place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let bob_l1b = g_place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let _bob_l2 = g_place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);
    let _bob_l3 = g_place(&mut ctx, BOB, 1, PRICE + 2 * TICK, QTY, 0, 0);

    // Book views while three ask levels exist — must not fold either.
    let c_book_views = read_commitment(&mut ctx);
    let asks = getBookPricesCall::abi_decode_returns(&dex_view(
        &mut ctx,
        &getBookPricesCall {
            marketId: MARKET_ID,
            side: 1,
        }
        .abi_encode(),
    ))
    .unwrap();
    assert_eq!(asks, vec![PRICE, PRICE + TICK, PRICE + 2 * TICK]);
    let bids = getBookPricesCall::abi_decode_returns(&dex_view(
        &mut ctx,
        &getBookPricesCall {
            marketId: MARKET_ID,
            side: 0,
        }
        .abi_encode(),
    ))
    .unwrap();
    assert_eq!(bids, vec![PRICE - 5 * TICK], "signed buy must be resting");
    let l1_queue = getBookLevelCall::abi_decode_returns(&dex_view(
        &mut ctx,
        &getBookLevelCall {
            marketId: MARKET_ID,
            side: 1,
            price: PRICE,
        }
        .abi_encode(),
    ))
    .unwrap();
    assert_eq!(l1_queue, vec![FixedBytes(bob_l1a), FixedBytes(bob_l1b)]);
    assert_eq!(
        read_commitment(&mut ctx),
        c_book_views,
        "book views must not fold the commitment"
    );

    // FOK that cannot fully fill (book depth is 4×QTY up to $102) and a
    // PostOnly that would cross — both revert under a rolled-back
    // checkpoint and must leave the commitment untouched.
    dex_call_expect_revert(
        &mut ctx,
        ALICE,
        &placeOrderCall {
            marketId: MARKET_ID,
            side: 0,
            price: PRICE + 2 * TICK,
            quantity: 5 * QTY,
            orderType: 0,
            tif: 2, // FOK
            clientOrderId: FixedBytes::default(),
                        flags: 0,
        }
        .abi_encode(),
        "FOK order cannot be fully filled",
    );
    dex_call_expect_revert(
        &mut ctx,
        ALICE,
        &placeOrderCall {
            marketId: MARKET_ID,
            side: 0,
            price: PRICE,
            quantity: QTY,
            orderType: 0,
            tif: 3, // PostOnly
            clientOrderId: FixedBytes::default(),
                        flags: 0,
        }
        .abi_encode(),
        "PostOnly order would match",
    );

    // Market taker (orderType = 1, price ignored): fills exactly L1's
    // head; the tail survives through the early-exit save_ask_level.
    let _alice_taker1 = g_place(&mut ctx, ALICE, 0, 0, QTY, 1, 1);
    assert_eq!(
        order_status(&mut ctx, bob_l1a),
        DELETED,
        "L1 head maker must be filled (delete-on-terminal → removed from map)"
    );
    assert_eq!(
        order_status(&mut ctx, bob_l1b),
        OrderStatus::Open as u8,
        "L1 tail maker must survive the buy-side early-exit"
    );
    let _alice_sweep = g_place(&mut ctx, ALICE, 0, PRICE + 2 * TICK, 3 * QTY, 0, 0);

    // Market order against the now-empty ask book: the full remainder
    // expires on the market path (execute_market_order →
    // cancel_unfilled_remainder), with no book or balance writes.
    let mkt = g_place(&mut ctx, ALICE, 0, 0, QTY, 1, 1);

    // PostOnly ask rests away from the market, then the only sell-side
    // user cancel (remove_ask_price + refresh_best_ask + save_ask_level +
    // save_sell_orders + sell-side margin release).
    let po_ask = g_place(&mut ctx, BOB, 1, PRICE + 10 * TICK, QTY, 0, 3);
    dex_call(
        &mut ctx,
        BOB,
        &cancelOrderCall {
            orderId: po_ask.into(),
            marketId: MARKET_ID,
        }
        .abi_encode(),
    );

    // GTC that rests, then explicit cancel; cancelling again must revert.
    let gtc = g_place(&mut ctx, ALICE, 0, PRICE - 10 * TICK, QTY, 0, 0);
    dex_call(
        &mut ctx,
        ALICE,
        &cancelOrderCall {
            orderId: gtc.into(),
            marketId: MARKET_ID,
        }
        .abi_encode(),
    );
    dex_call_expect_revert(
        &mut ctx,
        ALICE,
        &cancelOrderCall {
            orderId: gtc.into(),
            marketId: MARKET_ID,
        }
        .abi_encode(),
        // delete-on-terminal: the first cancel deleted the record, so re-cancelling it is now
        // "order not found" (was "not cancellable" when the Cancelled record lingered). Both
        // reject the replay.
        "order not found",
    );

    // IOC with no crossing liquidity → Expired.
    let ioc = g_place(&mut ctx, ALICE, 1, PRICE + 50 * TICK, QTY, 0, 1);

    // Signed cancel of the signed resting buy.
    dex_call(&mut ctx, CAROL, &signed_cancel_input(&sk, signed_buy));

    // Phase 7 — isolated margin ops.
    dex_call(
        &mut ctx,
        ALICE,
        &addPositionMarginCall {
            marketId: MARKET_ID,
            amount: 500_000,
        }
        .abi_encode(),
    );

    // Phase 8 — funding: one mid-epoch sample, then cross the epoch
    // boundary (FundingRateComputed + cumulative-index step).
    dex_call(
        &mut ctx,
        ORACLE,
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: PRICE,
            timestamp: 30,
        }
        .abi_encode(),
    );
    dex_call(
        &mut ctx,
        ORACLE,
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: PRICE,
            timestamp: 3_615,
        }
        .abi_encode(),
    );
    // Stale oracle update (same aligned timestamp): accepted but ignored —
    // it must not write, hence not fold.
    let c_stale = read_commitment(&mut ctx);
    dex_call(
        &mut ctx,
        ORACLE,
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: PRICE + TICK,
            timestamp: 3_616,
        }
        .abi_encode(),
    );
    assert_eq!(
        read_commitment(&mut ctx),
        c_stale,
        "ignored stale oracle update must not fold"
    );

    // Margin op AFTER the epoch boundary → settle_position_funding runs
    // with a nonzero index delta (lazy funding settlement on ALICE's long).
    dex_call(
        &mut ctx,
        ALICE,
        &removePositionMarginCall {
            marketId: MARKET_ID,
            amount: 250_000,
        }
        .abi_encode(),
    );

    // Phase 9 — insurance fund, crash, liquidation.
    dex_call(
        &mut ctx,
        ADMIN,
        &depositInsuranceFundCall { amount: 50_000_000 }.abi_encode(),
    );
    dex_call(
        &mut ctx,
        ADMIN,
        &withdrawInsuranceFundCall { amount: 1_000_000 }.abi_encode(),
    );
    dex_call_expect_revert(
        &mut ctx,
        BOB,
        &withdrawInsuranceFundCall { amount: 1 }.abi_encode(),
        "not admin",
    );

    // ALICE leaves a resting bid AND a resting ask so the liquidation's
    // cancel-all clears both book sides. The ask is fully offset by her
    // 4×QTY long, so it reserves no margin — only the maker fee.
    let alice_resting_bid = g_place(&mut ctx, ALICE, 0, PRICE - 30 * TICK, QTY, 0, 0);
    let alice_resting_ask = g_place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);

    // BOB quotes only 3×QTY of closing liquidity — resting BEFORE the crash so
    // the auto-liquidation sweep has it to close against: the sweep closes 3×QTY
    // through the book and settles the residual QTY at mark price.
    let bob_bid = g_place(&mut ctx, BOB, 0, PRICE - 20 * TICK, 3 * QTY, 0, 0);

    // Crash: index $100 → $70; ALICE's 3x long drops under maintenance and the
    // sweep inside updateIndexPrice liquidates her automatically (liquidator = 0x0).
    // Depth note: at 3x the liquidation boundary sits at a 27% drop (equity
    // `margin − loss` vs the `notional/6` threshold), so $80 — which liquidated the
    // pre-cap 5x long exactly — no longer does. $70 clears it with margin rather than
    // sitting on the boundary, so the scenario is not knife-edge. BOB's bid at $80 is
    // still crossable (the golden market runs with the band disabled), so the sweep
    // keeps closing 3×QTY through the book and settling the residual at mark.
    dex_call(
        &mut ctx,
        ORACLE,
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: PRICE - 30 * TICK,
            timestamp: 3_630,
        }
        .abi_encode(),
    );

    // The sweep already closed ALICE, so a manual liquidate now finds no position.
    dex_call_expect_revert(
        &mut ctx,
        CAROL,
        &liquidateCall {
            user: ALICE,
            marketId: MARKET_ID,
        }
        .abi_encode(),
        "liquidate: no open position",
    );

    // Phase 9b — CAROL funds up; BOB quotes two same-price bids and CAROL's
    // taker sell consumes exactly the head (closing BOB's residual short),
    // so the tail survives via the SELL-side matcher early-exit.
    dex_call(
        &mut ctx,
        CAROL,
        &depositCall {
            amount: U256::from(10_000_000u64),
        }
        .abi_encode(),
    );
    dex_call(
        &mut ctx,
        CAROL,
        &transferToPerpCall { amount: 5_000_000 }.abi_encode(),
    );
    let bob_head_bid = g_place(&mut ctx, BOB, 0, PRICE - 20 * TICK, QTY, 0, 0);
    let bob_tail_bid = g_place(&mut ctx, BOB, 0, PRICE - 20 * TICK, QTY, 0, 0);
    let carol_close = g_place(&mut ctx, CAROL, 1, PRICE - 20 * TICK, QTY, 0, 0);
    assert_eq!(
        order_status(&mut ctx, bob_head_bid),
        DELETED,
        "head bid must be filled by CAROL's taker sell (delete-on-terminal → removed)"
    );
    let tail_queue = getBookLevelCall::abi_decode_returns(&dex_view(
        &mut ctx,
        &getBookLevelCall {
            marketId: MARKET_ID,
            side: 0,
            price: PRICE - 20 * TICK,
        }
        .abi_encode(),
    ))
    .unwrap();
    assert_eq!(
        tail_queue,
        vec![FixedBytes(bob_tail_bid)],
        "tail bid must survive the sell-side early-exit"
    );

    // Phase 9c — a resting SELL against a NON-FLAT position, priced at the ASSUMING PRICE.
    //
    // This scenario had no such shape, which is why its BusinessSnapshot was structurally blind
    // to how `Ask` is priced: BOB's surviving tail bid is a BUY (a long order's Assuming Price IS
    // its limit price — no markup), and everyone else ends flat. Two calls fix that:
    //
    //   1. an oracle bump that lifts the MARK above the book. A resting sell must sit above the
    //      best bid, and the last print is AT the touch, so `ROUND_UP(lastTraded × 1.0015)` is
    //      only 15 bps above it — under this market's $1 tick on an $80 asset there is no
    //      placeable price in that gap. The `Mark` branch of `max(Last × 1.0015, Mark, limit)`
    //      is the reachable one, and it needs a mark above the book.
    //   2. CAROL — SHORT QTY, so a sell INCREASES her exposure and is genuinely charged — rests
    //      a sell inside the mark.
    //
    // Nobody is liquidated by the bump (ALICE and BOB are flat; CAROL's equity at the new mark
    // is far above her maintenance requirement), so the sweep is a no-op and only the mark, the
    // index blobs and CAROL's derived requirement move.
    dex_call(
        &mut ctx,
        ORACLE,
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: PRICE - 10 * TICK,
            timestamp: 3_645,
        }
        .abi_encode(),
    );
    let carol_ask = g_place(&mut ctx, CAROL, 1, PRICE - 19 * TICK, QTY, 0, 0);
    assert_eq!(
        order_status(&mut ctx, carol_ask),
        OrderStatus::Open as u8,
        "CAROL's ask must REST (it is above the surviving 80-tick bid), not fill"
    );

    // Phase 9d — a CONDITIONAL order, placed and left UNTRIGGERED.
    //
    // Without this the commitment is structurally blind to the whole algo family: the trigger
    // index blobs, the per-user algo list and the three new `Order` fields would never appear
    // in the write stream, so a change to any of them could not move the golden.
    //
    // CAROL is SHORT here, so her reduce-only stop is a BUY, and it fires on the mark RISING
    // to the trigger — which puts it in the UP index and exercises the direction that the
    // other half of the pair does not. The trigger sits well above the current mark on purpose:
    // the point is to pin an order that is PLACED and INDEXED but NOT fired, because a fired
    // one leaves no algo state behind to commit to.
    let carol_stop = dex_call(
        &mut ctx,
        CAROL,
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: 0, // BUY — the closing side of a short
            orderType: 1, // STOP_MARKET
            triggerPrice: PRICE + 20 * TICK,
            price: 0,
            quantity: QTY,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: 1, // reduceOnly
        }
        .abi_encode(),
    );
    assert_eq!(carol_stop.len(), 32, "placeAlgoOrder returns the algo id");

    // Phase 10 — solvent exit + api-key delete (empty-blob fold).
    dex_call(
        &mut ctx,
        BOB,
        &transferFromPerpCall {
            amount: 500_000_000,
        }
        .abi_encode(),
    );
    dex_call(
        &mut ctx,
        BOB,
        &withdrawCall {
            amount: U256::from(500_000_000u64),
        }
        .abi_encode(),
    );
    dex_call(&mut ctx, ALICE, &revokeApiKeyCall { keyId: 0 }.abi_encode());

    // Hand over admin last, after all fee-sink activity (the snapshot reads
    // ADMIN's account explicitly, so the handover does not disturb it).
    // Auth + zero-address gates first, under rolled-back checkpoints.
    dex_call_expect_revert(
        &mut ctx,
        ADMIN,
        &transferAdminCall {
            newAdmin: Address::ZERO,
        }
        .abi_encode(),
        "cannot be zero address",
    );
    dex_call_expect_revert(
        &mut ctx,
        BOB,
        &transferAdminCall { newAdmin: BOB }.abi_encode(),
        "caller is not admin",
    );
    dex_call(
        &mut ctx,
        ADMIN,
        &transferAdminCall {
            newAdmin: NEW_ADMIN,
        }
        .abi_encode(),
    );

    // View stretch #2 — pure reads (under #16d the slot is untouched until the block-end
    // finalize below, so it stays at genesis throughout the scenario).
    dex_view(
        &mut ctx,
        &getAveragePremiumIndexCall {
            marketId: MARKET_ID,
        }
        .abi_encode(),
    );
    let snapshot = take_snapshot(
        &mut ctx,
        &ScenarioOrderIds {
            signed_buy,
            gtc,
            ioc,
            mkt,
            po_ask,
            alice_resting_bid,
            alice_resting_ask,
            bob_bid,
            bob_tail_bid,
            carol_close,
            carol_ask,
        },
    );
    // #16d: the commitment is folded ONCE at block end. The snapshot above was read while the
    // overlay was intact; now harvest the net delta and finalize it onto 0x1003, exactly as the
    // block executor does (take_perp_delta → finalize_block_commitment) before the state root.
    let delta = ctx.journal_mut().take_perp_delta();
    storage::finalize_block_commitment(&mut ctx, &delta).unwrap();
    let commitment = read_commitment(&mut ctx);

    (B256::from(commitment.to_be_bytes::<32>()), snapshot)
}

/// Order IDs collected while the scenario runs, queried by the snapshot.
struct ScenarioOrderIds {
    signed_buy: [u8; 32],
    gtc: [u8; 32],
    ioc: [u8; 32],
    mkt: [u8; 32],
    po_ask: [u8; 32],
    alice_resting_bid: [u8; 32],
    alice_resting_ask: [u8; 32],
    bob_bid: [u8; 32],
    bob_tail_bid: [u8; 32],
    carol_close: [u8; 32],
    carol_ask: [u8; 32],
}

fn take_snapshot(ctx: &mut TestCtx, ids: &ScenarioOrderIds) -> BusinessSnapshot {
    let alice_pos = getPositionCall::abi_decode_returns(&dex_view(
        ctx,
        &getPositionCall {
            user: ALICE,
            marketId: MARKET_ID,
        }
        .abi_encode(),
    ))
    .unwrap();
    let bob_pos = getPositionCall::abi_decode_returns(&dex_view(
        ctx,
        &getPositionCall {
            user: BOB,
            marketId: MARKET_ID,
        }
        .abi_encode(),
    ))
    .unwrap();
    let carol_pos = getPositionCall::abi_decode_returns(&dex_view(
        ctx,
        &getPositionCall {
            user: CAROL,
            marketId: MARKET_ID,
        }
        .abi_encode(),
    ))
    .unwrap();
    let alice_acct = getAccountCall::abi_decode_returns(&dex_view(
        ctx,
        &getAccountCall { user: ALICE }.abi_encode(),
    ))
    .unwrap();
    let bob_acct = getAccountCall::abi_decode_returns(&dex_view(
        ctx,
        &getAccountCall { user: BOB }.abi_encode(),
    ))
    .unwrap();
    let carol_acct = getAccountCall::abi_decode_returns(&dex_view(
        ctx,
        &getAccountCall { user: CAROL }.abi_encode(),
    ))
    .unwrap();
    let admin_acct = getAccountCall::abi_decode_returns(&dex_view(
        ctx,
        &getAccountCall { user: ADMIN }.abi_encode(),
    ))
    .unwrap();
    let insurance_fund = getInsuranceFundCall::abi_decode_returns(&dex_view(
        ctx,
        &getInsuranceFundCall {}.abi_encode(),
    ))
    .unwrap();
    let market_fee_total = getMarketFeeTotalCall::abi_decode_returns(&dex_view(
        ctx,
        &getMarketFeeTotalCall {
            marketId: MARKET_ID,
        }
        .abi_encode(),
    ))
    .unwrap();
    let mark_price = getMarkPriceCall::abi_decode_returns(&dex_view(
        ctx,
        &getMarkPriceCall {
            marketId: MARKET_ID,
        }
        .abi_encode(),
    ))
    .unwrap();
    let funding = getFundingStateCall::abi_decode_returns(&dex_view(
        ctx,
        &getFundingStateCall {
            marketId: MARKET_ID,
        }
        .abi_encode(),
    ))
    .unwrap();
    let bob_erc20 = storage::load_erc20_balance(ctx, USDC_ADDRESS, BOB).unwrap();

    BusinessSnapshot {
        alice_position: (alice_pos.amount, alice_pos.vQuoteBalance, alice_pos.margin),
        bob_position: (bob_pos.amount, bob_pos.vQuoteBalance, bob_pos.margin),
        carol_position: (carol_pos.amount, carol_pos.vQuoteBalance, carol_pos.margin),
        alice_account: (alice_acct.usdcBalance, alice_acct.availableBalance),
        bob_account: (bob_acct.usdcBalance, bob_acct.availableBalance),
        carol_account: (carol_acct.usdcBalance, carol_acct.availableBalance),
        bob_erc20,
        admin_perp_wallet: admin_acct.availableBalance,
        insurance_fund,
        market_fee_total,
        mark_price,
        funding: (funding.fundingRate, funding.nextFundingTime),
        signed_buy_status: order_status(ctx, ids.signed_buy),
        gtc_cancelled_status: order_status(ctx, ids.gtc),
        ioc_status: order_status(ctx, ids.ioc),
        mkt_expired_status: order_status(ctx, ids.mkt),
        po_ask_cancelled_status: order_status(ctx, ids.po_ask),
        liq_cancelled_bid_status: order_status(ctx, ids.alice_resting_bid),
        liq_cancelled_ask_status: order_status(ctx, ids.alice_resting_ask),
        bob_bid_status: order_status(ctx, ids.bob_bid),
        bob_tail_bid_status: order_status(ctx, ids.bob_tail_bid),
        carol_close_status: order_status(ctx, ids.carol_close),
        carol_ask_status: order_status(ctx, ids.carol_ask),
    }
}

// ── The guards ─────────────────────────────────────────────────────────

#[test]
fn commitment_golden_scenario() {
    let (commitment, snapshot) = run_golden_scenario();
    // Printed for the (re-)pin procedure — visible with --nocapture or on failure.
    println!("golden commitment = {commitment}");
    println!("golden snapshot = {snapshot:#?}");
    assert_eq!(
        commitment, GOLDEN_COMMITMENT,
        "perp write-stream commitment drifted: SAFE refactors must keep it \
         bit-identical; only CHAIN changes may re-pin (see module docs)"
    );
    assert_eq!(
        snapshot,
        expected_snapshot(),
        "business end-state drifted — semantics changed, not just the hash"
    );
}

#[test]
fn commitment_golden_deterministic() {
    let first = run_golden_scenario();
    let second = run_golden_scenario();
    assert_eq!(first.0, second.0, "commitment must be deterministic");
    assert_eq!(
        first.1, second.1,
        "business end-state must be deterministic"
    );
}
