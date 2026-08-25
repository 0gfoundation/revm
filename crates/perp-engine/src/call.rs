//! The call shell: selector routing, flat gas, the EOA depth gate, dispatch, and
//! revert-payload encoding. Extracted verbatim from the precompile entry point — the
//! revm layer keeps only a thin `PrecompileResult` adapter over [`run_perp_dex_call`].

use std::{collections::HashMap, sync::OnceLock};

use alloy_sol_types::SolCall;
use primitives::{Address, Bytes, U256};

use perp_core::PerpError;

use crate::host::PerpHost;
use crate::{
    account::{
        run_deposit, run_get_account, run_get_api_key, run_get_api_keys,
        run_get_user_fee_rates, run_register_api_key, run_revoke_api_key,
        run_set_user_fee_rates, run_transfer_from_perp, run_transfer_to_perp, run_withdraw,
    },
    batch,
    errors,
    margin_view::{run_get_account_margin, run_get_margin_info, run_get_position_risk},
    interface::IPerpDex::{
        addMarketCall, addPositionMarginCall, batchCancelOrdersCall,
        batchCancelOrdersSignedCall, batchPlaceOrdersCall, batchPlaceOrdersSignedCall,
        cancelOrderCall, cancelOrderSignedCall, depositCall, depositInsuranceFundCall,
        getAccountCall, getAccountMarginCall, getAdminCall, getApiKeyCall, getApiKeysCall,
        getAveragePremiumIndexCall, getBookLevelCall, getBookPricesCall, getFundingStateCall,
        getIndexPriceCall, getInsuranceFundCall, getMarginInfoCall, getMarginTiersCall,
        getMarkPriceCall, getMarketCall,
        getMarketFeeTotalCall, getMarketManagerAddressCall, getOpenOrdersCall,
        getOracleAddressCall, getOrderCall, getPositionCall, getPositionRiskCall,
        getSymbolConfigCall,
        getUserFeeRatesCall,
        initAdminCall, liquidateCall, placeOrderCall, placeOrderSignedCall, registerApiKeyCall,
        removePositionMarginCall, revokeApiKeyCall, setLeverageCall, setLeverageSignedCall,
        setMarginTiersCall, setMarketManagerAddressCall, setOracleAddressCall,
        setUserFeeRatesCall,
        transferAdminCall, transferFromPerpCall, transferToPerpCall, updateIndexPriceCall,
        updateMarketCall, withdrawCall, withdrawInsuranceFundCall,
    },
    risk::{
        run_add_market, run_add_position_margin, run_deposit_insurance_fund, run_get_admin,
        run_get_average_premium_index, run_get_funding_state, run_get_index_price,
        run_get_insurance_fund, run_get_margin_tiers, run_get_mark_price, run_get_market,
        run_get_market_manager, run_get_oracle_address, run_get_position, run_get_symbol_config,
        run_init_admin, run_liquidate, run_remove_position_margin, run_set_leverage,
        run_set_leverage_signed, run_set_margin_tiers, run_set_market_manager,
        run_set_oracle_address, run_transfer_admin, run_update_index_price, run_update_market,
        run_withdraw_insurance_fund,
    },
    trading::{
        run_batch_cancel_orders, run_batch_cancel_orders_signed, run_batch_place_orders,
        run_batch_place_orders_signed, run_cancel_order, run_cancel_order_signed,
        run_get_book_level, run_get_book_prices, run_get_market_fee_total, run_get_open_orders,
        run_get_order, run_place_order, run_place_order_signed,
    },
    CANCEL_ORDER_GAS, PLACE_ORDER_GAS,
};

/// Output of one perp call through the shell — the engine-side mirror of revm's
/// `PrecompileOutput` (same field meanings; the adapter converts 1:1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerpOutput {
    /// Gas charged for the call (the flat table cost, or the batch envelope).
    pub gas_used: u64,
    /// ABI-encoded return data, or the `Error(string)` payload when `reverted`.
    pub bytes: Bytes,
    /// Whether the call reverted cleanly (business reject).
    pub reverted: bool,
}

impl PerpOutput {
    /// Successful call.
    pub fn new(gas_used: u64, bytes: Bytes) -> Self {
        Self { gas_used, bytes, reverted: false }
    }
    /// Clean revert carrying an ABI-encoded reason.
    pub fn new_reverted(gas_used: u64, bytes: Bytes) -> Self {
        Self { gas_used, bytes, reverted: true }
    }
}

// ── Selector table ────────────────────────────────────────────────────────────

/// `(gas_cost, can_be_called_in_static_context)`
///
/// For the `batch*` selectors `gas_cost` is only the **envelope floor** ([`batch::BASE_BATCH_GAS`]):
/// it drives the early `gas_cost > gas_limit` rejection and the static-context check, while the real
/// charge `BASE_BATCH_GAS + N * unit` is computed from the pre-decode array length in
/// [`run_perp_dex_call`] and is what lands in the returned [`PerpOutput`].
static SELECTORS: OnceLock<HashMap<[u8; 4], (u64, bool)>> = OnceLock::new();

pub(crate) fn selectors_map() -> &'static HashMap<[u8; 4], (u64, bool)> {
    SELECTORS.get_or_init(|| {
        let mut m = HashMap::new();
        // Admin
        m.insert(initAdminCall::SELECTOR, (30_000, false));
        m.insert(transferAdminCall::SELECTOR, (30_000, false));
        m.insert(getAdminCall::SELECTOR, (5_000, true));
        // Account
        //
        // ── THE `+10_000` ON EVERY SELECTOR THAT EMITS `AccountBalanceChanged` EXACTLY ONCE ──
        //
        // Publishing one snapshot folds `margin_view::index_account_wallet_balances` over the user's
        // market index. Charging nothing for it would leave these selectors buying a fold PLUS their
        // writes for less than a read selector pays for a fold alone — and `transferToPerp(1)` /
        // `transferFromPerp(1)` are freely spammable, so the brake really would weaken. So: one
        // snapshot-fold added to each selector whose emission count is exactly 1.
        //
        // **The surcharge is 10_000, halved from the 20_000 it was.** It was set at one
        // `getAccount`-equivalent when the event carried the whole eleven-field roll-up and the emit
        // path genuinely ran `index_account_scalars`. The payload is now three BALANCES, and the fold
        // behind it is a different, much cheaper walk:
        //
        //     per market      wide fold (getAccount)      snapshot fold (now)
        //     loads           2  {market, position}       1  {position}
        //     maint. margin   ≤8-band tier walk           —
        //     unrealized PnL  calc_value_i64 + add        —
        //     ooIM            open_order_margin           —
        //
        // At `MAX_USER_MARKETS` = 16 that is ≤ 18 `_ref` loads against ≤ 33, with the per-market
        // arithmetic gone entirely: 0.55× the loads and none of the derived math. `getAccount` stays
        // at 20_000 because it still buys the wide fold, so the ratio anchors the surcharge at half of
        // it. Rounding the load ratio up to 1/2 rather than down is the deliberately conservative
        // direction, and it keeps the surcharge inside the table's "walks a per-user list" family
        // instead of dropping it to the 5_000 scalar-getter tier — this fold does still walk ≤ 16
        // markets, which is the whole reason `getAccount` was moved out of that tier.
        //
        //     deposit               70_000 → 60_000    withdraw             70_000 → 60_000
        //     transferToPerp        40_000 → 30_000    transferFromPerp     40_000 → 30_000
        //     addPositionMargin     50_000 → 40_000    removePositionMargin 50_000 → 40_000
        //     depositInsuranceFund  50_000 → 40_000    withdrawInsuranceFund 50_000 → 40_000
        //     setLeverage           40_000 → 30_000    setLeverageSigned    40_000 → 30_000
        //
        // Every selector in this group is a NON-TRADING path, so it is still served by the coalescing
        // drain — one snapshot per touched user per call
        // (`storage::flush_account_snapshots`) — and "emission count is exactly 1" remains a property
        // of the selector rather than a per-write coincidence: each of these touches one account. (The
        // TRADING paths no longer go through the drain; they publish per fill / per order. That does
        // not reach these entries — see the `placeOrder` note below for the one it does reach.)
        // `addPositionMargin` / `removePositionMargin` were flagged as OVER-paying even at 20_000
        // (they went from two folds to one under coalescing and nothing was clawed back); halving the
        // surcharge collects that correction in passing rather than as a separate decision.
        //
        // FLAT per selector throughout. Dynamic or per-event metering for this precompile was
        // rejected outright, so the number is one constant per selector regardless of how many users
        // a call ends up publishing for.
        //
        // ⚠️ DELIBERATELY NOT RAISED: `placeOrder` / `cancelOrder` (and therefore the batch per-item
        // units, which are defined as those constants so they cannot drift), `liquidate`,
        // `updateIndexPrice`. Reasons, per selector, are on those entries. Note the direction of the
        // count changed with the per-event granularity: these used to emit strictly FEWER events than
        // when their price was last examined (which is why none of them moved DOWN), and a
        // fill-heavy call now emits MORE — one row per maker FILL instead of one per distinct maker.
        // That is a constant factor on a term these prices already under-model by an unbounded factor
        // (N settlements, N `Trade` + N `PositionChanged` logs, N position + N account writes, all at
        // one flat price), and the marginal snapshot is the cheapest thing in a fill: it reads
        // NOTHING, because the maker's wallet is already in the registry working copy and the
        // `Σ pos.margin` leg is one `i64` captured when that maker joined. FLAT per selector
        // throughout — dynamic or per-event metering for this precompile was rejected outright.
        m.insert(depositCall::SELECTOR, (60_000, false));
        m.insert(withdrawCall::SELECTOR, (60_000, false));
        m.insert(transferToPerpCall::SELECTOR, (30_000, false));
        m.insert(transferFromPerpCall::SELECTOR, (30_000, false));
        // `getAccount` is the account-level margin roll-up over the per-user market index, so it is
        // priced in the "walks a per-user list" tier (20_000) alongside `getMarginInfo` /
        // `getOpenOrders`, NOT the 5_000 scalar-getter tier it used to sit in.
        //
        // The LOADS did not change: at 5_000 it already walked the index through
        // `derived_available_balance` (≤ MAX_USER_MARKETS = 16 markets × {market, position} = ≤ 33
        // `_ref` loads — it was ≤ 66 with the `+ {MarketHot, sell list}` the read-time `Ask` re-fold
        // needed, before the R12 freeze put both aggregates in the position blob), and the
        // index-driven roll-up reaches exactly the same set — the added work is pure arithmetic (a ≤8-band maintenance
        // tier walk and `Σ isolatedWallet`, both off values already in hand). What changed is what
        // the selector BUYS: it now returns everything `getAccountMargin` returns, which is priced
        // at 50_000 for ≤64 ids, i.e. ~781/market. Leaving `getAccount` at 5_000 would make it the
        // cheap way to buy 16 markets of the same margin math — a 16× pricing gap between two
        // selectors doing identical per-market work. 20_000 closes it with margin: it is 1.6× the
        // rate `getAccountMargin` charges for the same 16 markets (50_000 × 16/64 = 12_500).
        // FLAT per selector, never per-item — dynamic metering for this precompile was rejected.
        //
        // ⚠️ RE-EXAMINED AND DELIBERATELY LEFT AT 20_000 when the return grew `positions[]` — one
        // full `getMarginInfo` row per market in place of the bare `uint64[] marketIds`. That is a
        // real change to what the selector BUYS, so it was decided rather than inherited:
        //
        // * The WALK did not move. Same market set, same `{market, position}` per member, same
        //   `≤ 33 _ref` loads. The rows are the fold's own `MarginInfo`s KEPT instead of discarded
        //   (`margin_view::fold_account_margin`) — no extra load, no second pass, no extra
        //   arithmetic. The dominant cost of this selector is untouched.
        // * What DID grow is the encoded output: 960 → 8_640 bytes at the 16-market cap (MEASURED),
        //   exactly 9×. That is one bounded allocation and ~8.6 KB of ABI encoding, orders of
        //   magnitude below a single storage load — and the caller pays the EVM's own memory-expansion
        //   and returndata gas for it separately, metered by the interpreter, not by us. The
        //   `MAX_USER_MARKETS` = 16 bound is what keeps it a constant rather than a lever.
        // * Raising it would push the wrong way. This shape exists so a backend stops issuing
        //   `getAccount` + N × `getMarginInfo`; that `1 + N` costs 20_000 + 16 × 20_000 = 340_000 and
        //   makes the node do ~64 loads instead of ~33 for the SAME answer. Pricing the consolidated
        //   call above the walk it actually performs would tax the cheaper access pattern and subsidise
        //   the more expensive one.
        // * The pre-existing asymmetry — `getAccount` doing up to 16× `getMarginInfo`'s per-market
        //   work at the same flat price — is UNCHANGED by this and is not settled here. It is a
        //   question about the whole "walks a per-user list" tier (`getOpenOrders` / `getBookPrices`
        //   are flat over per-user/per-book lists too), and it should be re-tiered as a tier if it is
        //   re-tiered at all, not opportunistically on the one selector a return-shape change touched.
        //
        // `getMarginInfo` likewise stays at 20_000: its addition is `entryPrice`, one division.
        //
        // ⚠️ RE-EXAMINED AND LEFT AT 20_000 AGAIN when every `positions[]` row grew
        // `liquidationPrice`. Same argument as the row array itself, one level down: the WALK does
        // not move, only the encoded output grows. The added work is a liquidation SEARCH per row,
        // and it performs ZERO storage loads — `margin_view::margin_info_of` runs it on the tier
        // table and the `(amount, vQuote, margin)` triple it is already holding, so the ≤ 33 `_ref`
        // loads that dominate this selector are untouched. What it costs is ~128 iterations of a
        // handful of `i128` multiplications plus a ≤ MAX_MARGIN_TIERS = 8 band walk — ~64 for the
        // domain bound and ~64 for the bisection — bounded by MAX_USER_MARKETS = 16 rows, i.e.
        // ~2_048 such iterations worst case against ~33 storage loads. A single load is orders of
        // magnitude more expensive than all of them together, which is the same reasoning that
        // priced `getPositionRisk` AT `getMarginInfo` rather than above it.
        // Raising it would push the wrong way for the same reason as above, and harder: the whole
        // point of the field being on the row is that a backend rendering N markets stops issuing
        // `getAccount` + N × `getPositionRisk` (20_000 + 16 × 20_000 = 340_000 and ~64 loads) for an
        // answer this one call already holds — and, unlike the `1 + N` shape, holds CONSISTENTLY.
        // The 20_000 is asserted by `margin_view_tests::get_account`, so a silent drift fails.
        m.insert(getAccountCall::SELECTOR, (20_000, true));
        m.insert(setUserFeeRatesCall::SELECTOR, (30_000, false));
        m.insert(getUserFeeRatesCall::SELECTOR, (5_000, true));
        m.insert(getMarketFeeTotalCall::SELECTOR, (5_000, true));
        // Roles
        m.insert(setMarketManagerAddressCall::SELECTOR, (30_000, false));
        m.insert(getMarketManagerAddressCall::SELECTOR, (5_000, true));
        m.insert(setOracleAddressCall::SELECTOR, (30_000, false));
        m.insert(getOracleAddressCall::SELECTOR, (5_000, true));
        // Market management
        m.insert(addMarketCall::SELECTOR, (100_000, false));
        m.insert(updateMarketCall::SELECTOR, (50_000, false));
        m.insert(getMarkPriceCall::SELECTOR, (5_000, true));
        m.insert(getMarketCall::SELECTOR, (5_000, true));
        // Risk table: setter mirrors addMarket's admin-write cost; getter is a plain view.
        m.insert(setMarginTiersCall::SELECTOR, (100_000, false));
        m.insert(getMarginTiersCall::SELECTOR, (5_000, true));
        // Leverage
        //
        // +10_000 each, and it is NO LONGER for a snapshot: `setLeverage` publishes NO
        // `AccountBalanceChanged` at all.
        //
        // The surcharge was added when the trigger widened to the position write, on the grounds that
        // `setLeverage` "now publishes exactly one". The note added in the same breath conceded the
        // problem — "the narrowed payload makes this snapshot carry no CHANGED field" — and it was
        // kept as the fail-safe direction. Adding `reason` removed that option: a row with no changed
        // field has no truthful reason, and every available label is one a consumer would act on. So
        // the write now takes `storage::save_position_leverage_only`, which marks nobody, and the
        // selector is silent for the same reason a pure placement and a pure cancel are — nothing it
        // can move is in the payload. Full argument on that function.
        //
        // The number stays at 30_000 anyway, and not out of inertia:
        // `risk::rebalance_order_margin_for_leverage` walks the per-user market index on its own
        // (`margin_view::derived_available_balance_with`), which is exactly the "walks a per-user
        // list" work this 10_000 tier prices. Dropping it back to 20_000 would weaken a spam brake to
        // reflect the removal of work that is still being done by a different line.
        m.insert(setLeverageCall::SELECTOR, (30_000, false));
        m.insert(setLeverageSignedCall::SELECTOR, (30_000, false));
        // `getSymbolConfig`: priced with `getPosition` (5_000), not with the derived margin views.
        // It is the same load set as `getPosition` — one position `_ref` and one market `_ref`, both
        // scalar blobs — and then pure in-register arithmetic: a walk of a table bounded by
        // MAX_MARGIN_TIERS = 8 that is already resident inside the `Market` it just read. No
        // per-user list, no order lists, no per-market walk, so none of the work the 20_000 tier
        // prices is present. FLAT, per selector.
        m.insert(getSymbolConfigCall::SELECTOR, (5_000, true));
        // Trading
        //
        // NOT raised for the `AccountBalanceChanged` snapshot, on purpose — and under the coalesced
        // trigger both of these now emit STRICTLY FEWER events than the price was set against:
        //
        // * `cancelOrder` emits NOTHING. A cancel moves no money (no escrow) and no position state —
        //   it only shrinks `Bid`/`Ask` — so it writes through `save_position_reservation_only`, which
        //   marks nobody. Binance is measured to push no `ACCOUNT_UPDATE` for a cancel either (R14).
        // * `placeOrder` emits nothing on a NON-CROSSING placement, for the same reason: resting is an
        //   aggregates-only write. (Two dead reasons have been recorded here before — "resting moves
        //   no money and writes no account", then "resting DOES emit, off the gate's own walk". The
        //   live reason is neither: resting genuinely moves `availableBalance`, and we are silent
        //   anyway because we are matching a measured venue. See
        //   `storage::mark_account_snapshot_dirty`.)
        //   A CROSSING placement emits **one snapshot per FILL plus one for the taker's order** —
        //   `F + 1` for F consumed maker orders, where `F ≥ N` for N distinct makers (it was `N + 1`
        //   under per-user coalescing, and `N + 4` under the original per-write emission). The
        //   marginal fill row costs **zero loads**: it is derived from the `MatchRegistry` working
        //   copy that fill just mutated, with `Σ pos.margin` reconstructed from one `i64` captured at
        //   `get_or_load`, so it is an `i64` add and a log. The taker's row is the only one that folds
        //   anything, and that fold is the three-balance one — a single account `_ref` load against
        //   state already resident.
        //   And `PLACE_ORDER_GAS` has always been flat over an
        //   unbounded-in-N match (N settlements, N `Trade` + N `PositionChanged` logs, N position + N
        //   account writes): the fold is a constant factor on a term this price already under-models.
        //   If the flat-vs-N mismatch is to be priced, it should be priced as such, not here.
        m.insert(placeOrderCall::SELECTOR, (PLACE_ORDER_GAS, false));
        m.insert(cancelOrderCall::SELECTOR, (CANCEL_ORDER_GAS, false));
        // Batch place / cancel: floor only — see the doc comment on SELECTORS.
        m.insert(
            batchCancelOrdersCall::SELECTOR,
            (batch::BASE_BATCH_GAS, false),
        );
        m.insert(
            batchPlaceOrdersCall::SELECTOR,
            (batch::BASE_BATCH_GAS, false),
        );
        m.insert(getOrderCall::SELECTOR, (5_000, true));
        m.insert(getOpenOrdersCall::SELECTOR, (20_000, true));
        m.insert(getBookPricesCall::SELECTOR, (20_000, true));
        m.insert(getBookLevelCall::SELECTOR, (20_000, true));
        // Positions
        m.insert(getPositionCall::SELECTOR, (5_000, true));
        // Derived margin views: pure reads (`can_be_static`). Priced like `getOpenOrders`
        // (20_000) because `getMarginInfo` walks the same two per-user order lists; the
        // account roll-up does that once per market id and is bounded by
        // `margin_view::MAX_MARGIN_INFO_MARKETS`.
        m.insert(getMarginInfoCall::SELECTOR, (20_000, true));
        // `getPositionRisk` = `getMarginInfo` plus `liquidationPrice`, so it is priced AT
        // `getMarginInfo`: the load set is identical (one market `_ref`, one position `_ref`, and
        // in debug the same two order lists), and the liquidation search adds ZERO loads — it
        // bisects `is_above_maintenance_margin` over ~64 iterations of a handful of `i128`
        // multiplications plus a walk of the MAX_MARGIN_TIERS = 8 table already resident in the
        // `Market` this call read, and ~64 more for its domain bound. That is orders of magnitude
        // below the one storage load it does not perform. Pricing it above `getMarginInfo` would
        // tax the strict superset and subsidise the call a client should be migrating off.
        // FLAT, per selector.
        m.insert(getPositionRiskCall::SELECTOR, (20_000, true));
        m.insert(getAccountMarginCall::SELECTOR, (50_000, true));
        // +10_000 each for the single `AccountBalanceChanged` snapshot they emit (see "Account").
        m.insert(addPositionMarginCall::SELECTOR, (40_000, false));
        m.insert(removePositionMarginCall::SELECTOR, (40_000, false));
        // Liquidation
        //
        // NOT raised: a liquidation emits one snapshot per maker FILL in the close sweep (those makers
        // are ordinary counterparties, so they get the per-fill rule), plus one per DISTINCT other
        // address from the drain — the liquidated user and each ADL counterparty, whose residual,
        // clearance fee and ADL legs all still coalesce into a single settled row each. The liquidated
        // user is deliberately NOT published at the close (`match_order` skips the taker emit when
        // `liquidation_close`): the close is one leg of a liquidation that goes on to charge a
        // clearance fee and possibly settle a residual, so a row there would be a half-liquidated
        // account. It is protocol-driven risk work whose flat price already spans an orderbook close
        // plus an ADL loop bounded only by `adl_budget` — the same flat-vs-unbounded structure as
        // `placeOrder`, and a zero-load row per fill does not move it.
        m.insert(liquidateCall::SELECTOR, (150_000, false));
        // API key management
        m.insert(registerApiKeyCall::SELECTOR, (30_000, false));
        m.insert(revokeApiKeyCall::SELECTOR, (20_000, false));
        m.insert(getApiKeyCall::SELECTOR, (5_000, true));
        m.insert(getApiKeysCall::SELECTOR, (10_000, true));
        // Signed order submission (relayer path)
        m.insert(placeOrderSignedCall::SELECTOR, (PLACE_ORDER_GAS, false));
        m.insert(cancelOrderSignedCall::SELECTOR, (CANCEL_ORDER_GAS, false));
        // Batch place / cancel (signed): floor only — see the doc comment on SELECTORS.
        m.insert(
            batchCancelOrdersSignedCall::SELECTOR,
            (batch::BASE_BATCH_GAS, false),
        );
        m.insert(
            batchPlaceOrdersSignedCall::SELECTOR,
            (batch::BASE_BATCH_GAS, false),
        );
        // Insurance Fund
        // +10_000 each for the single `AccountBalanceChanged` snapshot they emit (see "Account").
        m.insert(depositInsuranceFundCall::SELECTOR, (40_000, false));
        m.insert(withdrawInsuranceFundCall::SELECTOR, (40_000, false));
        m.insert(getInsuranceFundCall::SELECTOR, (5_000, true));
        // Index price
        //
        // NOT raised: the in-process liquidation sweep here can emit many snapshots (one per maker fill
        // across a cap-50 sweep, plus one per distinct other address from the drain), and this selector
        // was ALREADY the largest flat-price-vs-work gap in the table for exactly that reason. It is
        // oracle/admin-only, not user-spammable, and pricing the sweep is a separate decision from
        // this event.
        //
        // STILL NOT RAISED for the out-of-band expiry sweep added alongside the liquidation sweep
        // (`risk::run_out_of_band_expiry_sweep`), and the numbers are why. Its cap is
        // `MAX_BAND_EXPIRIES_PER_UPDATE` = 64 orders, each costing roughly what one `cancelOrder`
        // costs INSIDE the engine (a book detach, a per-user entry removal, an aggregates-only
        // position write, an order delete, one log — no matching, no settlement, no account write).
        // Priced at the `CANCEL_ORDER_GAS` rate that would be 64 × 80_000 = 5_120_000; the sweep it
        // sits next to is already 50 liquidations, each of which does a cancel-ALL plus a full
        // market-order close through the book plus settlement plus up to `ADL_BUDGET_PER_UPDATE`
        // = 128 ADL fills. So the addition is a small fraction of a gap this entry already accepts
        // deliberately, on a selector no user can call. Raising 50_000 by 5.1M to "cover" the new
        // term while leaving the far larger existing term uncovered would be arbitrary; the honest
        // statement is that this selector is subsidised by design and the new work does not change
        // that. FLAT, per selector — dynamic or per-item metering for this precompile was rejected
        // outright.
        m.insert(updateIndexPriceCall::SELECTOR, (50_000, false));
        m.insert(getIndexPriceCall::SELECTOR, (5_000, true));
        m.insert(getFundingStateCall::SELECTOR, (5_000, true));
        m.insert(getAveragePremiumIndexCall::SELECTOR, (5_000, true));
        m
    })
}

// ── Revert encoding ───────────────────────────────────────────────────────────

/// Encode `msg` as a Solidity `Error(string)` revert payload so that ethers.js
/// can decode `e.reason` from the returned bytes.
///
/// Layout: selector(4) | offset(32) | length(32) | data(padded to 32)
/// selector = keccak256("Error(string)")[0..4] = 0x08c379a0
fn encode_revert_string(msg: &str) -> primitives::Bytes {
    const SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
    let msg_bytes = msg.as_bytes();
    let msg_len = msg_bytes.len();
    let padded = (msg_len + 31) / 32 * 32;

    let mut data = Vec::with_capacity(4 + 32 + 32 + padded);
    data.extend_from_slice(&SELECTOR);
    // offset to the string data = 0x20 (32)
    let mut buf = [0u8; 32];
    buf[31] = 0x20;
    data.extend_from_slice(&buf);
    // string length
    let mut buf = [0u8; 32];
    buf[24..].copy_from_slice(&(msg_len as u64).to_be_bytes());
    data.extend_from_slice(&buf);
    // string bytes, right-padded with zeros
    data.extend_from_slice(msg_bytes);
    data.resize(4 + 32 + 32 + padded, 0u8);

    primitives::Bytes::from(data)
}


// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run_perp_dex_call<H: PerpHost>(
    input_bytes: &[u8],
    gas_limit: u64,
    caller: Address,
    _value: U256,
    is_static: bool,
    context: &mut H,
) -> Result<PerpOutput, PerpError> {
    // #16d: the perp commitment is folded ONCE at block end (`finalize_block_commitment` from the
    // block executor), not per call — so there is no per-call commitment work in this entry point.
    let selector: [u8; 4] = input_bytes
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .ok_or(PerpError::StatefulInvalidInput)?;

    let base_gas_used = match selectors_map().get(&selector) {
        Some(&(gas_cost, can_be_static)) => {
            if gas_cost > gas_limit {
                return Err(PerpError::OutOfGas);
            }
            if is_static && !can_be_static {
                return Err(PerpError::StaticRestrictionViolation);
            }
            gas_cost
        }
        None => return Err(PerpError::StatefulInvalidInput),
    };

    // Dynamic-gas batch selectors. The table cost checked above is only the envelope FLOOR; the real
    // charge is `BASE_BATCH_GAS + N * unit`, where `N` is read PRE-DECODE from the calldata (this
    // decision has to precede ABI decoding). Done here — before the dispatch, before any write — so
    // an insufficient `gas_limit` is a clean `OutOfGas` with zero writes, exactly like the fixed-cost
    // check above. `None` means "not a batch selector", or "the length word is untrustworthy", in
    // which case only the floor is charged and the handler produces the clean revert.
    let mut charged_gas = base_gas_used;
    if let Some(cost) = batch::batch_dynamic_gas(selector, input_bytes) {
        if cost > gas_limit {
            return Err(PerpError::OutOfGas);
        }
        charged_gas = cost;
    }

    // commit-only #23: EOA-direct calls only. Perp writes are commit-only (the per-op undo is
    // being removed), so no enclosing frame that could revert AFTER a successful perp call may
    // exist. Frame depth: a top-level (tx-level) call executes at depth 1 (0 when unit tests
    // invoke this entry directly); ANY contract-mediated call — CALL or DELEGATECALL (which
    // spoofs caller==origin but still adds a frame) — is deeper and is rejected. The in-process
    // liquidation sweep is a plain function call inside this same frame, unaffected.
    if context.call_depth() > 1 {
        return Err(errors::perp_err("perpdex: EOA direct calls only"));
    }

    // commit-only #23 residual-write-then-error tripwire: snapshot the journal's perp-write counter
    // before dispatch. A call that ends REVERTED must not have written the overlay
    // (validate-then-apply); if it did, undo is gone and the write leaked. Diagnostic only.
    let writes_before = context.perp_write_count();

    // Open the call for `AccountBalanceChanged` coalescing: drop any marks a PREVIOUS call left
    // un-drained (it reverted, so it published nothing — see `storage::flush_account_snapshots`).
    // This has to be here and not at a tx/block boundary: the set lives in the live typed store,
    // which the journal keeps for the whole BLOCK.
    crate::storage::begin_perp_call(context);

    // Dispatch — note: no `?` here; errors are caught below and converted to
    // clean REVERT output so ethers.js can read `e.reason`.
    let result = match selector {
        // Admin
        s if s == initAdminCall::SELECTOR => run_init_admin(input_bytes, context),
        s if s == transferAdminCall::SELECTOR => run_transfer_admin(input_bytes, caller, context),
        s if s == getAdminCall::SELECTOR => run_get_admin(input_bytes, context),
        // Account
        s if s == depositCall::SELECTOR => run_deposit(input_bytes, caller, context),
        s if s == withdrawCall::SELECTOR => run_withdraw(input_bytes, caller, context),
        s if s == transferToPerpCall::SELECTOR => {
            run_transfer_to_perp(input_bytes, caller, context)
        }
        s if s == transferFromPerpCall::SELECTOR => {
            run_transfer_from_perp(input_bytes, caller, context)
        }
        s if s == getAccountCall::SELECTOR => run_get_account(input_bytes, context),
        s if s == setUserFeeRatesCall::SELECTOR => {
            run_set_user_fee_rates(input_bytes, caller, context)
        }
        s if s == getUserFeeRatesCall::SELECTOR => run_get_user_fee_rates(input_bytes, context),
        s if s == getMarketFeeTotalCall::SELECTOR => run_get_market_fee_total(input_bytes, context),
        // Roles
        s if s == setMarketManagerAddressCall::SELECTOR => {
            run_set_market_manager(input_bytes, caller, context)
        }
        s if s == getMarketManagerAddressCall::SELECTOR => {
            run_get_market_manager(input_bytes, context)
        }
        s if s == setOracleAddressCall::SELECTOR => {
            run_set_oracle_address(input_bytes, caller, context)
        }
        s if s == getOracleAddressCall::SELECTOR => run_get_oracle_address(input_bytes, context),
        // Market management
        s if s == addMarketCall::SELECTOR => run_add_market(input_bytes, caller, context),
        s if s == updateMarketCall::SELECTOR => run_update_market(input_bytes, caller, context),
        s if s == getMarkPriceCall::SELECTOR => run_get_mark_price(input_bytes, context),
        s if s == getMarketCall::SELECTOR => run_get_market(input_bytes, context),
        s if s == setMarginTiersCall::SELECTOR => {
            run_set_margin_tiers(input_bytes, caller, context)
        }
        s if s == getMarginTiersCall::SELECTOR => run_get_margin_tiers(input_bytes, context),
        // Leverage
        s if s == setLeverageCall::SELECTOR => run_set_leverage(input_bytes, caller, context),
        s if s == setLeverageSignedCall::SELECTOR => run_set_leverage_signed(input_bytes, context),
        s if s == getSymbolConfigCall::SELECTOR => run_get_symbol_config(input_bytes, context),
        // Trading
        s if s == placeOrderCall::SELECTOR => run_place_order(input_bytes, caller, context),
        s if s == cancelOrderCall::SELECTOR => run_cancel_order(input_bytes, caller, context),
        s if s == batchCancelOrdersCall::SELECTOR => {
            run_batch_cancel_orders(input_bytes, caller, context)
        }
        s if s == batchCancelOrdersSignedCall::SELECTOR => {
            run_batch_cancel_orders_signed(input_bytes, context)
        }
        s if s == batchPlaceOrdersCall::SELECTOR => {
            run_batch_place_orders(input_bytes, caller, context)
        }
        s if s == batchPlaceOrdersSignedCall::SELECTOR => {
            run_batch_place_orders_signed(input_bytes, context)
        }
        s if s == getOrderCall::SELECTOR => run_get_order(input_bytes, context),
        s if s == getOpenOrdersCall::SELECTOR => run_get_open_orders(input_bytes, context),
        s if s == getBookPricesCall::SELECTOR => run_get_book_prices(input_bytes, context),
        s if s == getBookLevelCall::SELECTOR => run_get_book_level(input_bytes, context),
        // Positions
        s if s == getPositionCall::SELECTOR => run_get_position(input_bytes, context),
        s if s == getMarginInfoCall::SELECTOR => run_get_margin_info(input_bytes, context),
        s if s == getPositionRiskCall::SELECTOR => run_get_position_risk(input_bytes, context),
        s if s == getAccountMarginCall::SELECTOR => run_get_account_margin(input_bytes, context),
        s if s == addPositionMarginCall::SELECTOR => {
            run_add_position_margin(input_bytes, caller, context)
        }
        s if s == removePositionMarginCall::SELECTOR => {
            run_remove_position_margin(input_bytes, caller, context)
        }
        // Liquidation
        s if s == liquidateCall::SELECTOR => run_liquidate(input_bytes, caller, context),
        // API key management
        s if s == registerApiKeyCall::SELECTOR => {
            run_register_api_key(input_bytes, caller, context)
        }
        s if s == revokeApiKeyCall::SELECTOR => run_revoke_api_key(input_bytes, caller, context),
        s if s == getApiKeyCall::SELECTOR => run_get_api_key(input_bytes, context),
        s if s == getApiKeysCall::SELECTOR => run_get_api_keys(input_bytes, context),
        // Signed order submission (relayer path)
        s if s == placeOrderSignedCall::SELECTOR => run_place_order_signed(input_bytes, context),
        s if s == cancelOrderSignedCall::SELECTOR => run_cancel_order_signed(input_bytes, context),
        // Insurance Fund
        s if s == depositInsuranceFundCall::SELECTOR => {
            run_deposit_insurance_fund(input_bytes, caller, context)
        }
        s if s == withdrawInsuranceFundCall::SELECTOR => {
            run_withdraw_insurance_fund(input_bytes, caller, context)
        }
        s if s == getInsuranceFundCall::SELECTOR => run_get_insurance_fund(input_bytes, context),
        // Index price
        s if s == updateIndexPriceCall::SELECTOR => {
            run_update_index_price(input_bytes, caller, context)
        }
        s if s == getIndexPriceCall::SELECTOR => run_get_index_price(input_bytes, context),
        s if s == getFundingStateCall::SELECTOR => run_get_funding_state(input_bytes, context),
        s if s == getAveragePremiumIndexCall::SELECTOR => {
            run_get_average_premium_index(input_bytes, context)
        }
        // Unreachable: the selector-table gate above rejects unknown selectors before
        // dispatch. Loud fatal (not a panic) if map and match ever drift apart.
        _ => Err(PerpError::Fatal("perpdex: selector dispatch mismatch".into())),
    };

    // ── The coalesced account snapshots: one per STILL-marked user, in address order, LAST ──
    //
    // SUCCESS PATH ONLY, and this is the whole reason it sits between the dispatch and the output:
    // a reverted call must publish nothing (its perp writes are validate-then-apply, so there is
    // nothing to report), and a snapshot taken after the dispatch is a settled account state rather
    // than one of the half-updated intermediates the old per-write emission produced. TOP-LEVEL by
    // construction — the depth gate above rejects `> 1`, so this entry point cannot be nested and no
    // inner call can drain early; the liquidation sweep and the batch drivers are plain calls in this
    // same frame and coalesce into this drain. See `storage::flush_account_snapshots`.
    //
    // "STILL-marked" is the granularity change: the trading paths publish at their own economic
    // events — one per FILL for a maker, one per ORDER for a taker — and clear those users' marks
    // (`storage::clear_account_snapshot_mark`), so what reaches this drain is the non-trading writes
    // plus the incidental fee recipient. A user who moved AGAIN after their direct emit stays marked
    // and is drained here too, which is the fail-safe direction.
    //
    // A drain failure is folded back into `result` so it takes the same clean-revert path a dispatch
    // error takes (and trips the same `perp_write_count` witness). Only a pathological account can
    // get there — the same exposure the write-site emission had.
    let result = result.and_then(|bytes| {
        crate::storage::flush_account_snapshots(context)?;
        Ok(bytes)
    });

    match result {
        Ok(bytes) => {
            // The snapshots are FREE — the call is charged only `charged_gas` (the flat per-selector
            // cost, or `BASE_BATCH_GAS + N * unit` for a batch selector). Never per event, never
            // dynamic: per-event metering for this precompile was rejected outright.
            Ok(PerpOutput::new(charged_gas, bytes))
        }
        // Fatal + shell errors propagate as-is (halt / precompile-level error, never a revert).
        Err(
            e @ (PerpError::Fatal(_)
            | PerpError::OutOfGas
            | PerpError::StaticRestrictionViolation
            | PerpError::StatefulInvalidInput),
        ) => Err(e),
        // All other errors become a clean REVERT with an ABI-encoded reason
        // string, so ethers.js exposes `e.reason` to the caller.
        Err(error) => {
            // Tripwire: a reverting call that WROTE the overlay is a residual write-then-error
            // (commit-only #23 — the write leaks with no undo). Record the offending selector +
            // count into a global so the exact path can be surfaced (read via
            // [`last_perp_write_then_revert`]); diagnostic only, not a halt.
            let writes_after = context.perp_write_count();
            if writes_after != writes_before {
                let sel = u32::from_be_bytes(selector);
                LAST_WRITE_THEN_REVERT_SELECTOR.store(sel, core::sync::atomic::Ordering::Relaxed);
                PERP_WRITE_THEN_REVERT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            Ok(PerpOutput::new_reverted(
                charged_gas,
                encode_revert_string(&error.to_string()),
            ))
        }
    }
}

/// commit-only #23 tripwire state: the last selector that reverted AFTER writing the overlay
/// (a residual write-then-error), and how many such events have occurred process-wide.
pub static LAST_WRITE_THEN_REVERT_SELECTOR: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
pub static PERP_WRITE_THEN_REVERT_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Returns `(count, last_selector)` of residual write-then-error reverts seen so far.
pub fn last_perp_write_then_revert() -> (u64, u32) {
    (
        PERP_WRITE_THEN_REVERT_COUNT.load(core::sync::atomic::Ordering::Relaxed),
        LAST_WRITE_THEN_REVERT_SELECTOR.load(core::sync::atomic::Ordering::Relaxed),
    )
}
