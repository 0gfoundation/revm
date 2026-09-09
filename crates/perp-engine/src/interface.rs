//! Solidity ABI definitions for the PerpDEX precompile.
use alloy_sol_types::sol;

sol! {
    interface IPerpDex {
        // ── Admin ─────────────────────────────────────────────────────────────
        /// Initialise the admin address. Can only be called once (when no admin is set).
        function initAdmin(address admin) external;
        /// Transfer admin role to a new address. Only callable by current admin.
        function transferAdmin(address newAdmin) external;
        /// Query the current admin address. Returns zero if not yet initialised.
        function getAdmin() external view returns (address admin);

        // ── Account ────────────────────────────────────────────────────────
        /// Deposit USDC into the user's spot balance inside the DEX.
        function deposit(uint256 amount) external;
        /// Withdraw USDC from the user's spot balance back to their wallet.
        function withdraw(uint256 amount) external;
        /// Move USDC from spot balance into the perp trading wallet.
        function transferToPerp(uint64 amount) external;
        /// Move USDC from the perp trading wallet back to spot balance.
        function transferFromPerp(uint64 amount) external;
        /// **THE per-market margin row, and the ONLY one.** Binance-shaped, computed on demand,
        /// stores nothing.
        ///
        /// # THIS COMMENT IS THE FIELD-BY-FIELD CONTRACT — every surface points here
        ///
        /// Two selectors return this struct and there is exactly ONE encoder behind both
        /// (`margin_view::AccountPositionRow::to_abi`), so it is also the one place the fields are
        /// specified:
        ///
        /// * `getPositionRisk(user, marketId)` — ONE row. The single-market accessor, the analogue
        ///   of Binance's `/positionRisk`.
        /// * `getAccount(user).positions[]` — one row PER MARKET in the user's index, in ONE call
        ///   and ONE state. **Prefer it** over `1 + N` reads; see `getAccount` for why `1 + N`
        ///   cannot be made consistent.
        ///
        /// Every field of every row comes from one `margin_view::MarginInfo`, produced by
        /// `margin_view::margin_info_of` — the single implementation of the per-market margin
        /// arithmetic. There is no second derivation to drift from and no second encoder to drop a
        /// field, and `margin_view_tests::get_account_positions_agree_with_get_position_risk_field_for_field`
        /// asserts the two ACCESS PATHS agree positionally over all 17 numbers, on a live
        /// multi-market index, rather than trusting it.
        ///
        /// ⚠️ **REPLACES `getMarginInfo`**, which returned these same numbers minus `marketId` and
        /// `liquidationPrice` as a flat tuple, through a second ABI encoder. It is DELETED, not
        /// deprecated. Migration: `getPositionRisk` for one market, `getAccount` for an account.
        ///
        /// Unlike Binance's own `v3 account.positions[]` — which reports `notional`,
        /// `initialMargin` and `maintMargin` while omitting every input needed to check them (no
        /// mark, no entry, no leverage, no bid/ask notional), forcing two more endpoint calls — the
        /// six INPUT fields are returned, so a client can recompute every derived value locally and
        /// byte-exactly.
        ///
        /// Formula source: `misc/binance-margin-verified-model.md` §1.1/§2 and
        /// `misc/binance-v3-account-balance-field-reference.md` §4 (Binance USDⓈ-M mainnet,
        /// ISOLATED + ONE-WAY, measured to 8 decimals).
        ///
        /// Identity:
        ///   marketId        which market this row is for. On `getPositionRisk` it echoes the
        ///                   argument, which is not redundancy: it makes the row SELF-DESCRIBING, so
        ///                   one row and a row out of `positions[]` are interchangeable to the code
        ///                   that consumes them.
        ///
        /// Inputs:
        ///   markPrice       market's current mark, in `priceDecimals` fixed-point units.
        ///   positionAmt     signed net position (base units). Positive = long.
        ///   vQuoteBalance   virtual quote balance; `entryPrice = -vQuoteBalance / positionAmt`,
        ///                   and that quotient is also returned outright as `entryPrice` below.
        ///   leverage        the position's leverage setting (never 0 — floored at 1).
        ///   bidNotional     `Bid` = Σ over the user's resting BUYS in this market of
        ///                   `qty × that order's LIMIT price` (NOT mark), each term floored to
        ///                   quote units exactly as the engine's own aggregate fold floors it.
        ///                   A LONG order's Assuming Price IS its limit price, so no markup.
        ///   askNotional     `Ask` = Σ over resting SELLS of `qty × max(T, that order's LIMIT
        ///                   price)`, where `T = max(ROUND_UP(lastTraded × 1.0015), markPrice)` is
        ///                   the Assuming-Price floor. A SHORT order resting at or below `T` is
        ///                   charged at `T`, not at its own price — Binance's vendor Cost formula,
        ///                   measured on mainnet (run9 admission probes; R10 measured the reported
        ///                   `askNotional / qty == limit × 1.0015` for a sell resting below `T`).
        ///                   `T` is resolved ONCE, when the order is placed, and the term is FROZEN
        ///                   at `max(T, limit)` for that order's whole life — never re-resolved
        ///                   (MEASURED, R12; `types::OrderEntry::assuming_price`, and see the
        ///                   `margin_view` module docs). So this field does NOT move with the mark:
        ///                   it changes only when an order of this user's is placed, cancelled or
        ///                   filled. Fields DERIVED from it do move with the mark, but through `N`
        ///                   (the live position notional) — never through this one.
        ///
        /// Derived (Binance formulas, Binance rounding):
        ///   entryPrice            Binance `entryPrice` — the position's volume-weighted average
        ///                         entry, `-vQuoteBalance / positionAmt` scaled into
        ///                         `priceDecimals` units. **0 when `positionAmt == 0`** (a flat
        ///                         position has no entry; it is not a price of zero). Derived, not
        ///                         stored: `vQuoteBalance` is the accumulated `-(qty × price)` and
        ///                         this is its per-unit inverse, so the two are consistent by
        ///                         construction. It comes from `math::calc_entry_price` — the SAME
        ///                         function the `PositionChanged` event uses for its own
        ///                         `entryPrice`, so the event and the row agree digit for digit and
        ///                         there is no second derivation to drift.
        ///   notional              `trunc(|positionAmt| × markPrice)` — TRUNCATED, and every
        ///                         field below uses this truncated value, not raw mark.
        ///   unrealizedProfit      `positionAmt × (markPrice − entryPrice)`, TRUNCATED TOWARD
        ///                         ZERO (not floor). Computed here as
        ///                         `signedNotional + vQuoteBalance`, where `signedNotional` is
        ///                         `notional` carrying `positionAmt`'s sign.
        ///   isolatedMargin        `isolatedWallet + unrealizedProfit`, where `isolatedWallet` is
        ///                         the field two below. This is position EQUITY at mark, not a
        ///                         balance: it may sit below `isolatedWallet`, and it may go
        ///                         negative.
        ///   positionInitialMargin `ROUND_UP(notional / leverage)` — ROUND_UP, not truncate.
        ///   initialMargin         `ROUND_UP( max(|N + Bid|, |N − Ask|) / leverage )` — the
        ///                         JOINT requirement over position AND resting orders, a genuine
        ///                         `max()` (neither branch always wins). `N` is the SIGNED
        ///                         notional: the two branches are "exposure if every buy fills"
        ///                         and "exposure if every sell fills".
        ///                         NOTE this deliberately mixes bases — `N` at mark, `Bid` at
        ///                         limit price, `Ask` at the Assuming Price. That is Binance's
        ///                         formula, and this row reports Binance's numbers.
        ///   openOrderInitialMargin  `initialMargin − positionInitialMargin`. Computed as that
        ///                         DIFFERENCE OF TWO ROUND_UPs, never as a single round-up of a
        ///                         difference — the convenience form
        ///                         `ROUND_UP(max(0, Bid, Ask − 2N) / L)` is not equivalent at
        ///                         1 ulp, because `ceil(a) − ceil(b) != ceil(a − b)`.
        ///   maintMargin           maintenance margin at `notional` under this market's tier
        ///                         table (`getMarginTiers`). The tier table affects THIS field
        ///                         and `liquidationPrice` only — it is NOT an input to
        ///                         `initialMargin`, which divides by the position's own `leverage`,
        ///                         uncapped.
        ///   isolatedWallet        the position's own allocated margin BALANCE — `isolatedMargin`
        ///                         one level down, before unrealised PnL. The only margin quantity
        ///                         physically held anywhere: it was moved out of the perp wallet
        ///                         when the position opened, and it moves only on open /
        ///                         add-or-remove-margin / close, never with the mark.
        ///                         `openOrderInitialMargin` by contrast is escrowed NOWHERE — it
        ///                         is subtracted arithmetically at the admission gate.
        ///                         WAS named `positionMargin` (ours) until it took Binance's name;
        ///                         bindings generated before that are looking for the old one.
        ///   liquidationPrice      the mark price at which this position **IS** liquidatable — for a
        ///                         LONG the GREATEST such price, for a SHORT the LEAST.
        ///                         **Conservative by construction: at this price liquidation
        ///                         FIRES**, and one tick the other way (`+1` for a long, `−1` for a
        ///                         short) it does not. `0` when there is no such price, which covers
        ///                         three states — a FLAT position, a fully-funded LONG
        ///                         (`vQuoteBalance + margin >= 0`, i.e. 1× or lower: as the price
        ///                         falls both notional and requirement go to zero together, so it
        ///                         never becomes liquidatable), and a SHORT so small that its
        ///                         notional cannot outgrow its funding anywhere in the representable
        ///                         price range. `0` is the ABSENCE of a price, not a price of zero;
        ///                         `positionAmt` in the same row separates the flat case.
        ///                         ⚠️ IT IS A SEARCH, NOT A FORMULA, AND THAT IS DELIBERATE.
        ///                         `math::calc_liquidation_price` bisects
        ///                         `math::is_above_maintenance_margin` — the predicate `liquidate()`
        ///                         and the auto-liquidation sweep actually enforce — over the mark
        ///                         price, which is exact by construction and needs no closed form. A
        ///                         closed form would have to solve a FIXED POINT (the maintenance
        ///                         rate depends on the tier, the tier on the notional, the notional
        ///                         on the price being solved for), reproduce `maintenance_margin`'s
        ///                         integer SLICE form (whose subtractive twin is off by one at 53.1%
        ///                         of tier boundaries), and get both signs of a truncate-toward-zero
        ///                         right. See that function for the monotonicity argument the
        ///                         bisection rests on.
        ///                         ⚠️ WHY IT IS ON-CHAIN. It was computed OFF-chain against a flat
        ///                         1% maintenance ratio while this chain liquidates off the market's
        ///                         margin-tier table — `1/6` on today's default tier, sixteen times
        ///                         that rate. Real liquidation therefore fired far earlier than
        ///                         users were shown. Deriving it here kills the whole class, because
        ///                         it now comes from the same code path `liquidate()` enforces.
        ///                         It costs ZERO storage loads: the search runs on the tier table
        ///                         and the `(amount, vQuote, margin)` triple `margin_info_of` is
        ///                         already holding, which is why carrying it on every row of the
        ///                         bulk path left `getAccount`'s flat gas unchanged.
        ///
        /// A user with no position and no orders reads back all zeros with `leverage = 1`. Both
        /// selectors REVERT if the market does not exist — the decimals and the tier table are
        /// required INPUTS, so a fabricated zero market would report a full row of plausible zeros.
        /// (`getPosition` differs here on purpose; see it.)
        struct AccountPosition {
            uint64 marketId;         // which market this row is for
            // ── inputs: every field below is recomputable from these six ──
            uint64 markPrice;
            int64  positionAmt;
            int64  vQuoteBalance;
            uint64 leverage;
            uint64 bidNotional;
            uint64 askNotional;
            // ── derived ──
            uint64 entryPrice;       // `-vQuoteBalance / positionAmt`; 0 when flat
            uint64 notional;
            int64  unrealizedProfit;
            int64  isolatedMargin;
            uint64 positionInitialMargin;
            uint64 openOrderInitialMargin;
            uint64 initialMargin;
            uint64 maintMargin;
            int64  isolatedWallet;
            // ── derived, ours ──
            uint64 liquidationPrice;
        }
        /// Query a user's spot USDC plus the full account-level margin roll-up, over the markets
        /// the user is actually active in. **INDEX-DRIVEN**: the caller passes no market list —
        /// this walks the per-user market index (`umkt`, at most MAX_USER_MARKETS = 16 ids), which
        /// is exactly the support of every sum below (a market the user has left contributes
        /// `N = Bid = Ask = 0`). So unlike `getAccountMargin`, whose totals are only as complete as
        /// the list handed to it, these totals are COMPLETE by construction.
        ///
        /// This is the `GET /fapi/v3/account` account-level scalar set, less two fields we
        /// deliberately do not have (see the end of this comment). Every balance-like field is
        /// `int64` and **UNCLAMPED**.
        ///
        /// ⚠️ THE OLD `uint64 availablePerpBalance` IS GONE, replaced by `int64 availableBalance`.
        /// It floored at 0, which made a deficit invisible through this call: an operator polling
        /// only `getAccount` could not distinguish "exactly covered" from "under-covered by any
        /// amount". That is the identical trap Binance's own clamped `availableBalance` has, and
        /// `misc/binance-v3-account-balance-field-reference.md` §1 is explicit that their field
        /// therefore **cannot be used to tell whether an account is under-covered** (measured:
        /// reported `0.00000000` against a true `−0.00085981`). We had no reason to inherit it.
        ///
        ///   usdcBalance                 spot / withdrawal-layer USDC held inside the DEX. This is
        ///                               the SPOT side and is NOT part of any total below — it is
        ///                               also the balance `withdraw` actually gates on.
        ///   totalWalletBalance          Binance `totalWalletBalance` — the GROSS perp wallet:
        ///                               `totalCrossWalletBalance + Σ isolatedWallet`. THIS call
        ///                               walks the index for it (it is loading every position blob
        ///                               anyway); `AccountBalanceChanged` reads the same number off
        ///                               the stored `Σ pos.margin` aggregate
        ///                               (`UserAccount::total_position_margin`) with no walk at all,
        ///                               and the two are cross-checked against each other in debug
        ///                               builds on every snapshot published off the store. (A maker's
        ///                               per-fill snapshot is published off the match working copy
        ///                               instead, and `MatchRegistry::flush` asserts THAT formula
        ///                               converges on the same stored aggregate.) (Not to be confused with
        ///                               the deleted `total_perp_collateral` "TC" field, which also
        ///                               carried the open-order ESCROW and therefore had to be
        ///                               rewritten on every order rest and cancel; `Σ pos.margin`
        ///                               does not move on either.)
        ///   totalCrossWalletBalance     Binance `totalCrossWalletBalance` — our stored
        ///                               `perpWalletBalance`, verbatim and signed. Position margin
        ///                               has physically left it; the open-order requirement has not.
        ///   totalMarginBalance          Binance `totalMarginBalance` =
        ///                               `totalWalletBalance + totalUnrealizedProfit`. Total account
        ///                               EQUITY. GROSS-based, so it is NOT
        ///                               `totalCrossWalletBalance + totalUnrealizedProfit`.
        ///   totalUnrealizedProfit       Σ `positions[].unrealizedProfit`.
        ///   totalInitialMargin          Σ `initialMargin` (== the next two, summed).
        ///   totalPositionInitialMargin  Σ `positionInitialMargin`.
        ///   totalOpenOrderInitialMargin Σ `openOrderInitialMargin`. Escrowed NOWHERE — derived.
        ///   totalMaintMargin            Σ `maintMargin`. Below it, positions are liquidatable.
        ///   availableBalance            `totalCrossWalletBalance − totalOpenOrderInitialMargin`,
        ///                               signed and UNCLAMPED. The exact quantity every admission
        ///                               gate enforces (`derived_available_balance`), so it is the
        ///                               real headroom, not a parallel reporting number. CROSS-based
        ///                               on purpose: the silos are already out of the cross wallet.
        ///                               A negative value is not by itself distress — a mark move
        ///                               alone reaches it, and resting orders are not torn down for
        ///                               it — but new risk-INCREASING actions are refused until it
        ///                               recovers.
        ///   positions                   one row PER MARKET in the index, ascending — the exact
        ///                               market set every total above was summed over, carrying the
        ///                               per-market numbers those totals are made OF. At most
        ///                               MAX_USER_MARKETS = 16 rows, so the array is bounded by the
        ///                               same constant that bounds the walk. Empty for an account
        ///                               with no positions and no resting orders, in which case
        ///                               every total is 0 and the call still succeeds — an empty
        ///                               array is the "no markets" signal, distinguishable from a
        ///                               market set that happened to sum to zero.
        ///                               Every total above is SELF-CHECKABLE against it without a
        ///                               second call: `Σ positions[i].<field>` must reproduce the
        ///                               matching `total…` field exactly.
        ///                               ⚠️ **REPLACES `uint64[] marketIds`.** That array was this
        ///                               same set with every per-market number deleted, so it is a
        ///                               projection of this one —
        ///                               `marketIds == [r.marketId for r in positions]` — and a
        ///                               caller that only wants the ids can still take it from here.
        ///                               Bindings generated before this are looking for the old
        ///                               name and will not find it.
        ///
        /// ⚠️ WHY THE ROWS ARE IN HERE AND NOT FETCHED PER MARKET: **`1 + N` READS CANNOT BE MADE
        /// CONSISTENT.** `getAccount` for the totals followed by one per-market row call per id was the
        /// only way to assemble a full account picture, and those `N + 1` calls are `N + 1` separate
        /// `eth_call`s that can land on DIFFERENT BLOCKS. The identity
        /// `totalWalletBalance == totalCrossWalletBalance + Σ isolatedWallet` then fails for a
        /// perfectly healthy account — the left side was read at block `n`, the silos at `n+k`, and a
        /// fill in between moved one of them. A backend seeing that mismatch cannot tell a race from
        /// a bug in this precompile, and the only fixes available to it are wrong (pin a block tag,
        /// and it serves stale balances; retry until the numbers agree, and it stalls on an active
        /// account). One call is ONE STATE: every field here, scalar and row, is read from the same
        /// store at the same block, so the identities hold by construction and a mismatch is
        /// unambiguously our bug.
        ///
        /// This costs nothing: the walk behind the totals ALREADY computes each row in full and
        /// discarded everything but the sums. No extra load, no second pass — see
        /// `margin_view::fold_account_margin`. The same holds for `liquidationPrice`, the one row
        /// field with no `total…` counterpart: its search consumes only the tier table and position
        /// triple that walk is already holding.
        ///
        /// ⚠️ NUMBERS ONLY — the rows deliberately do NOT mirror Binance's `positions[]` JSON
        /// literally, and three of its fields are absent on purpose. `symbol`: we key by
        /// `marketId`, and the backend maps id → symbol itself from the `MarketAdded` log (a string
        /// repeated in 16 rows is calldata spent on data the caller already has). `isolated`:
        /// always `true` — this venue has no cross mode. `positionSide`: always `BOTH` — one-way
        /// mode only. A backend serving a Binance-shaped `/account` fills those three in itself,
        /// from constants and its own market table.
        ///
        /// ⚠️ `totalCrossUnPnl` AND `maxWithdrawAmount` ARE PRESENT BUT DEGENERATE. Read this before
        /// using either — both were once deliberately omitted, and one of those reasons still bites.
        ///
        /// They are here for RESPONSE-SHAPE COMPATIBILITY: a client written against Binance's
        /// account payload can bind to this selector without a special case. That was an explicit
        /// product decision, taken knowing the objections below.
        ///
        /// * `totalCrossUnPnl` — ALWAYS EXACTLY ZERO, and permanently so. We are isolated-only;
        ///   there are no cross positions for it to sum. It is not "zero right now". **Do not build
        ///   a cross-vs-isolated split on it** — that split does not exist here. All unrealised PnL
        ///   is in `totalUnrealizedProfit`.
        ///   NAME: this was `crossUnPnl` until the rename below. Binance has BOTH spellings and
        ///   they live at different levels — `crossUnPnl` on an `assets[]` ROW, `totalCrossUnPnl`
        ///   as the ACCOUNT-LEVEL scalar. This is the account-level one, so the per-asset spelling
        ///   was simply wrong. Same class of error as the `walletBalance` collision in §7.1.2.
        /// * `maxWithdrawAmount` — `max(0, availableBalance)`, i.e. the most that
        ///   `transferFromPerp` will let out of the PERP wallet right now.
        ///   ⚠️ **THE NAME IS WRONG FOR OUR TWO-LAYER MODEL AND WE KEPT IT ANYWAY.** `withdraw()`
        ///   gates on `usdcBalance` — the SPOT layer — not on this number. So "max withdraw" here
        ///   does NOT tell you how much you can withdraw from the protocol; it tells you how much
        ///   you can move perp → spot. To actually withdraw you then need a second step whose
        ///   limit is `usdcBalance`. The name is kept only because Binance uses it for the
        ///   same-shaped field. **Prefer `availableBalance` in new code** — it is the same number
        ///   before the clamp, and it is not lying about which layer it describes.
        ///   The clamp matters: `availableBalance` is signed and can be negative (an
        ///   under-covered account), but "you may withdraw a negative amount" is meaningless, so
        ///   this field floors at zero. **The un-clamped truth stays visible in
        ///   `availableBalance` right next to it** — that is why clamping here costs no
        ///   information, and why it must NOT be done to `availableBalance` itself.
        ///
        /// One earlier objection has dissolved and is recorded so it is not re-raised: the docs
        /// note `maxWithdrawAmount == availableBalance` on Binance as an OBSERVATION over 69
        /// readings rather than a published formula, so mirroring it would have been presenting an
        /// extrapolation as a rule. That applies to predicting *Binance*. Here we are not
        /// predicting anything — we DEFINE the field as `max(0, availableBalance)`, so there is no
        /// extrapolation left in it.
        ///
        /// ⚠️ PORTING PITFALLS FROM §7 THAT WE DO NOT REPRODUCE.
        ///
        /// Two are structurally impossible for us, one took discipline:
        /// * §7.0 (the same semantic zero serialised three ways — `"0"`, `"0.00000000"`, `"0.000"`
        ///   — inside ONE object): IMPOSSIBLE. Every field here is a fixed-width ABI integer in
        ///   base units; zero has exactly one 32-byte encoding and there is no decimal-string
        ///   surface anywhere in this precompile to disagree with itself.
        /// * §7.1.1 (`unrealizedProfit` on one endpoint vs `unRealizedProfit` on another, same
        ///   quantity, so one shared deserialiser silently reads zero): DISCIPLINE, not structure —
        ///   nothing in `sol!` would catch a second spelling. We use the lowercase-`r`
        ///   `unrealizedProfit` / `totalUnrealizedProfit` on every selector, and there is no second
        ///   spelling in the ABI.
        /// * §7.1.2 (the same quantity under different names on different endpoints — Binance's
        ///   `balance` vs `walletBalance`): DISCIPLINE, and we had this bug. `getAccountMargin`
        ///   used to call the cross wallet `walletBalance`, colliding with Binance's name for the
        ///   GROSS wallet — an error of a full position's margin for anyone comparing them. Both
        ///   selectors now say `totalCrossWalletBalance` for that one quantity, and the gross one is
        ///   only ever `totalWalletBalance`.
        ///   **We had it a SECOND time and it survived longer:** `getAccount` returned the
        ///   account-level cross unrealised PnL under `crossUnPnl`, which is Binance's name for the
        ///   PER-ASSET row; the account-level scalar is `totalCrossUnPnl`. Renamed. It cost nothing
        ///   here only because the value is structurally zero — the same mistake on a live field is
        ///   what §7.1.2 is about. When adding a Binance-shaped field, check WHICH LEVEL the name
        ///   belongs to, not just that Binance uses it somewhere.
        /// * §7.1.3 (v3's `positions[]` reports derived quantities while deleting every input, so a
        ///   caller cannot self-check): WE DO NOT — and `positions[]` is now where we most visibly
        ///   do not. Each row's first six numbers after `marketId` are the raw INPUTS
        ///   (`markPrice, positionAmt, vQuoteBalance, leverage, bidNotional, askNotional`), so every
        ///   derived field in the row AND every total above it can be recomputed from this ONE
        ///   response, byte-exactly, with no second call. That round-trip is pinned by
        ///   `margin_view_tests::get_account_totals_equal_the_sum_of_per_market_position_risk`,
        ///   and the rows' agreement with the single-market accessor by
        ///   `margin_view_tests::get_account_positions_agree_with_get_position_risk_field_for_field`.
        function getAccount(address user) external view returns (
            uint256  usdcBalance,
            int64    totalWalletBalance,
            int64    totalCrossWalletBalance,
            int64    totalMarginBalance,
            int64    totalUnrealizedProfit,
            uint64   totalInitialMargin,
            uint64   totalPositionInitialMargin,
            uint64   totalOpenOrderInitialMargin,
            uint64   totalMaintMargin,
            int64    availableBalance,
            int64    totalCrossUnPnl,
            uint64   maxWithdrawAmount,
            AccountPosition[] positions
        );
        /// Set per-user trading fee rates in basis points. Only callable by admin.
        function setUserFeeRates(address user, uint64 makerFeeBps, uint64 takerFeeBps) external;
        /// Query per-user trading fee rates in basis points. Unset users default to zero.
        function getUserFeeRates(address user) external view returns (uint64 makerFeeBps, uint64 takerFeeBps);
        /// Query total trading fees collected for one market.
        function getMarketFeeTotal(uint64 marketId) external view returns (uint64 totalFee);

        // ── Market management (admin only) ─────────────────────────────────
        /// Register a new perpetual market.
        function addMarket(uint64 marketId, uint32 baseDecimals, uint32 priceDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, uint64 fundingInterval, int64 interestRate, uint32 liquidationFeeRateBps, uint64 initialMarkPrice, uint32 priceBandBps) external;
        /// Update mutable market parameters (tick/step/quantity/price limits, active flag, and funding config).
        /// baseDecimals and priceDecimals cannot be changed after creation.
        function updateMarket(uint64 marketId, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, bool active, uint64 fundingInterval, int64 interestRate, uint32 liquidationFeeRateBps, uint32 priceBandBps) external;
        /// Read the current mark price for a market.
        function getMarkPrice(uint64 marketId) external view returns (uint64 price);
        /// Read the configuration of a registered market. Reverts if the market does not exist.
        function getMarket(uint64 marketId) external view returns (
            uint32 baseDecimals,
            uint32 priceDecimals,
            uint64 tickSize,
            uint64 stepSize,
            uint64 minQuantity,
            uint64 maxQuantity,
            uint64 maxPrice,
            uint64 priceUpdateInterval,
            bool   active,
            uint64 fundingInterval,
            int64  interestRate,
            uint32 liquidationFeeRateBps
        );
        /// Query the current average premium index for the active funding epoch.
        function getAveragePremiumIndex(uint64 marketId) external view returns (int64 avgPremiumIndex, uint64 sampleCount);

        /// Replace a market's margin-tier table (admin or market manager).
        ///
        /// Two index-aligned parallel arrays (same shape as the batch calls): tier `i` is
        /// `{lowerBounds[i], maxLeverages[i]}`. Deliberately NOT part of `updateMarket`, so
        /// retuning tick/step/funding can never reset the risk table.
        ///
        /// Rejected (whole call, zero writes) unless ALL hold: the market exists; the two
        /// arrays are the same length; `1 <= length <= MAX_MARGIN_TIERS (8)`;
        /// `lowerBounds[0] == 0`; `lowerBounds` strictly increasing; every `maxLeverages[i]`
        /// in `1..=MAX_LEVERAGE_HARD_CAP (100)`; `maxLeverages` non-increasing.
        ///
        /// Tier 0's `maxLeverage` becomes the market's `setLeverage` cap. Existing positions
        /// are NOT re-checked: an over-levered position keeps running and is only refused
        /// when it next tries to OPEN.
        function setMarginTiers(uint64 marketId, uint64[] lowerBounds, uint32[] maxLeverages) external;
        /// Read a market's margin-tier table as the same two index-aligned arrays
        /// `setMarginTiers` takes. Reverts if the market does not exist.
        function getMarginTiers(uint64 marketId) external view returns (uint64[] lowerBounds, uint32[] maxLeverages);

        // ── Leverage ───────────────────────────────────────────────────────
        /// Set the leverage for the caller's position in a market.
        function setLeverage(uint64 marketId, uint64 leverage) external;
        /// Set leverage for `account`, authenticated by ed25519 signature.
        /// Message: "perpdex_v1_leverage"(19) || account(20) || marketId(8) || leverage(8)
        ///          || timestamp(8) || recvWindow(8) || keyId(1)
        function setLeverageSigned(
            address account,
            uint64 marketId,
            uint64 leverage,
            uint64 timestamp,
            uint64 recvWindow,
            uint8 keyId,
            bytes calldata signature
        ) external;
        /// A user's leverage in one market, together with the largest position the market's risk
        /// table permits AT that leverage. Binance's `/fapi/v1/symbolConfig` row, less the
        /// constants. Reverts if the market does not exist.
        ///
        ///   leverage          the position's `leverage` setting, `1` when unset — the same
        ///                     floored value `getPosition` and every `AccountPosition` row report, so this selector cannot disagree
        ///                     with them about which leverage is in force.
        ///   maxNotionalValue  the largest `|notional|` (quote units, at MARK, the same basis
        ///                     `AccountPosition.notional` reports) the tier table admits while
        ///                     `leverage` is in force: the upper bound of the highest tier whose
        ///                     `maxLeverage` is still `>= leverage`. **`0` MEANS UNBOUNDED** — that
        ///                     tier is the last one and its band runs to infinity. Every market
        ///                     today ships the single default tier `{0, 3}`, so every market today
        ///                     returns `0` at every legal leverage; the field only becomes
        ///                     informative once `setMarginTiers` installs a real table.
        ///
        /// ⚠️ WHY THIS IS A SELECTOR AND NOT `getMarginTiers` PLUS CLIENT ARITHMETIC. The chain
        /// enforces the tier table in ONE direction — given a notional, which leverage is still
        /// allowed (`math::max_leverage_for_notional`, the quantity `setLeverage` and every
        /// per-open guard compare against). `maxNotionalValue` is that table read BACKWARDS, and
        /// nothing in the engine needs the inverse, so a consumer wanting it had to reimplement the
        /// band walk from `getMarginTiers`. That is a second implementation of a rule this engine
        /// enforces, in a language the engine cannot test, and it drifts silently: the failure is a
        /// user shown a position size the chain will refuse (or worse, not shown one it will).
        /// `math::max_notional_for_leverage` is the inversion, it lives next to the forward lookup
        /// it inverts, and this selector is how it gets out.
        ///
        /// ⚠️ NUMBERS ONLY — `marginType` and `isAutoAddMargin` are deliberately ABSENT. Binance's
        /// row carries both; ours would be the constants `"ISOLATED"` and `false` for every user in
        /// every market forever (this venue has no cross mode and no auto-margin-add), and encoding
        /// a constant into a return value spends calldata on data the caller already has. The
        /// `AccountPosition` comment states the same rule for `symbol` / `isolated` /
        /// `positionSide`. A backend serving a Binance-shaped `/symbolConfig` fills them in itself.
        function getSymbolConfig(address user, uint64 marketId) external view returns (
            uint64 leverage,
            uint64 maxNotionalValue
        );

        // ── Trading ────────────────────────────────────────────────────────
        /// Place a limit or market order.
        /// side:      0 = Buy, 1 = Sell
        /// orderType: 0 = Limit, 1 = Market
        /// tif:       0 = GTC, 1 = IOC, 2 = FOK, 3 = PostOnly
        ///
        /// Only SIX of the eight `(orderType, tif)` pairs are products, and the engine holds them
        /// as ONE value internally (`types::OrderKind`):
        ///   * LIMIT takes all four TIFs.
        ///   * MARKET is *inherently* immediate-or-cancel, bounded by the market's price band. It
        ///     accepts `tif = 0` (GTC — the unset placeholder; Binance's own spot response echoes
        ///     this on a market order) or `tif = 1` (IOC, the explicit spelling), and both mean the
        ///     same thing: match from the best price inward, discard any remainder, never rest.
        ///   * MARKET + FOK (2) and MARKET + PostOnly (3) are REJECTED — neither names a product
        ///     that exists. Reverts with `placeOrder: tif not allowed for market order`.
        /// A market order always reports `tif = 1` (IOC) in `OrderPlaced`, whichever of the two
        /// accepted bytes was sent.
        /// clientOrderId: optional caller-assigned tracking ID; pass bytes16(0) if unused.
        ///   Emitted in events but not stored or validated on-chain.
        /// Returns a unique order ID.
        function placeOrder(uint64 marketId, uint8 side, uint64 price, uint64 quantity, uint8 orderType, uint8 tif, bytes16 clientOrderId) external returns (bytes32 orderId);
        /// Cancel an open order (caller must be the owner).
        /// marketId is accepted for ABI compatibility but ignored — orders are looked up globally by orderId.
        function cancelOrder(bytes32 orderId, uint64 marketId) external;
        /// Cancel up to MAX_BATCH_CANCEL (256) open orders in one call (caller must be the owner of each).
        ///
        /// Atomicity is **abort-forward**, not all-or-nothing: each id is attempted in calldata order
        /// and a per-item genuine reject (not found / not owner / not cancellable / unknown market)
        /// does NOT undo the items before it. The call returns Ok once the loop has begun.
        ///
        /// `statuses` = N concatenated 34-byte records, index-aligned to `orderIds`:
        ///   byte 0     tag       0 = Rejected, 1 = Accepted (cancelled), 3 = Aborted, 4 = NotAttempted
        ///   bytes 1..33 orderId  the input id, echoed
        ///   byte 33    reason    numeric reason code; 0 when accepted
        /// Reason codes are NUMERIC (never strings) — see `PerpBatchReason` in `perp_dex::batch`.
        ///
        /// Gas: BASE_BATCH + N * cancelOrder-cost, charged up front from the pre-decode array length.
        /// Reverts as a whole (zero writes) for: bad calldata, N == 0, N > 256, insufficient gas.
        function batchCancelOrders(bytes32[] orderIds) external returns (bytes statuses);
        /// One order of a batch placement — the same seven arguments `placeOrder` takes, minus the
        /// market-wide ones. All fields are STATIC, so `PlaceItem[]` encodes as an offset word, a
        /// length word, then N * 224 bytes inline (7 words per item, no per-element offset table).
        /// That fixed stride is what makes the pre-decode length bound in `BatchArrayLayout` sound.
        struct PlaceItem {
            uint64  marketId;
            uint8   side;       // 0 = Buy, 1 = Sell
            uint64  price;      // ignored for Market orders
            uint64  quantity;
            uint8   orderType;  // 0 = Limit, 1 = Market
            uint8   tif;        // 0 = GTC, 1 = IOC, 2 = FOK, 3 = PostOnly
                                // Market accepts only 0/1 — see `placeOrder` for the legal matrix
            bytes16 clientOrderId;
        }
        /// Place up to MAX_BATCH_PLACE (64) orders in one call, on behalf of the caller.
        ///
        /// Atomicity is **abort-forward**, not all-or-nothing: items run in strict calldata order and
        /// a per-item genuine reject (unknown/inactive market, bad side/orderType/tif, an illegal
        /// (orderType, tif) pair, qty or price
        /// out of range, PostOnly would cross, FOK unfillable, insufficient margin, K9
        /// open-into-insolvency) does NOT undo the items before it. The call returns Ok once the loop
        /// has begun. A rejected item is write-clean AND log-clean — it emits no `OrderPlaced`.
        ///
        /// Each item matches the book **as left by the previous item**, so self-crossing inside one
        /// batch is allowed and behaves exactly like submitting the items as separate transactions in
        /// that order.
        ///
        /// `statuses` = N concatenated 34-byte records, index-aligned to `orders`:
        ///   byte 0      tag      1 = Accepted (rests: Open/PartiallyFilled),
        ///                        2 = Accepted and terminal (fully Filled, or an IOC/FOK/market
        ///                            remainder that Expired — no resting record is left),
        ///                        0 = Rejected, 3 = Aborted, 4 = NotAttempted
        ///   bytes 1..33 orderId  the id the order was placed under; ZERO for tags 0/3/4 (a
        ///                        rejected item never consumed an id, so none exists)
        ///   byte 33     reason   numeric reason code; 0 when accepted
        /// Reason codes are NUMERIC (never strings) — see `PerpBatchReason` in `perp_dex::batch`;
        /// place-path rejects occupy the 16..=63 band. `statuses` is the outcome AT THE TIME the item
        /// ran: the taker wallet-cover path can auto-cancel an order an earlier item just rested, and
        /// the logs (`Trade` / `OrderCancelled` / `PositionChanged`) are the authority on final state.
        ///
        /// orderId derivation is the SAME per-user nonce chain `placeOrder` uses —
        /// keccak256(account || nonce) — and the nonce advances by exactly the number of items that
        /// consumed an id (accepted, plus an aborted item). A rejected item consumes nothing.
        ///
        /// Gas: BASE_BATCH + N * placeOrder-cost, charged up front from the pre-decode array length.
        /// Reverts as a whole (zero writes) for: bad calldata, N == 0, N > 64, insufficient gas.
        function batchPlaceOrders(PlaceItem[] orders) external returns (bytes statuses);
        /// Query order details.
        /// marketId is accepted for ABI compatibility but ignored — orders are looked up globally by orderId.
        function getOrder(bytes32 orderId, uint64 marketId) external view returns (
            address owner,
            uint64  marketId,
            uint8   side,
            uint64  price,
            uint64  quantity,
            uint64  filled,
            uint8   status
        );
        /// Query a user's open order entries in one market.
        /// Buy orders are returned first, sorted by price descending; sell orders follow, sorted by price ascending.
        function getOpenOrders(address user, uint64 marketId) external view returns (
            bytes32[] orderIds,
            uint8[]   sides,
            uint64[]  prices,
            uint64[]  remainingQuantities
        );
        /// Query one side's active price levels in matching priority order.
        /// side: 0 = Buy returns bid prices descending; 1 = Sell returns ask prices ascending.
        function getBookPrices(uint64 marketId, uint8 side) external view returns (
            uint64[] prices
        );
        /// Query the FIFO order-id queue at one price level.
        /// side: 0 = Buy reads the bid level; 1 = Sell reads the ask level.
        function getBookLevel(uint64 marketId, uint8 side, uint64 price) external view returns (
            bytes32[] orderIds
        );

        // ── Positions ─────────────────────────────────────────────────────
        /// Query a user's perpetual position in a market.
        ///
        /// `openOrderMargin` is the DERIVED open-order requirement this position's resting orders
        /// add — `AccountPosition`'s `openOrderInitialMargin`, repeated here so the common
        /// "position + what its orders cost me" query is one call. It occupies the slot the old
        /// `marginReserved` escrow field had, and answers the same question, but nothing is
        /// escrowed: it is recomputed from `(N, Bid, Ask, L)` on every read and moves when the
        /// mark moves. `0` when the market does not exist.
        function getPosition(address user, uint64 marketId) external view returns (
            int64  amount,
            int64  vQuoteBalance,
            int64  margin,
            uint64 openOrderMargin,
            uint64 leverage
        );

        // ── Derived margin view (Binance-shaped; pure read, stores nothing) ───
        //
        // Binance stores only a small ledger (`walletBalance`, per-position `isolatedWallet`,
        // `positionAmt`, `entryPrice`) and DERIVES every margin quantity on read. So do we: the
        // six escrow fields that used to be stored per position (`margin_reserved`,
        // `margin_reserved_notional`, `buy/sell_side_margin_reserved`,
        // `buy/sell_side_reserved_notional`) are deleted, and nothing is debited from the wallet
        // when an order rests.
        //
        // These views therefore report the numbers the ENGINE ITSELF enforces — there is one
        // ooIM definition, not a reported one and an enforced one. They move no money and write
        // no storage.
        //
        // The per-market row they report is `AccountPosition`, and ITS doc comment is the
        // field-by-field contract (formula source included) for every field in this section.
        // `getPositionRisk` returns one row; `getAccount` returns one per market. `getMarginInfo`
        // used to return the same numbers as a flat tuple through a second encoder and is DELETED.

        /// The full Binance `/positionRisk` row for one `(user, marketId)` — returned as ONE
        /// [`AccountPosition`], the very struct `getAccount().positions[]` is an array of.
        ///
        /// ⚠️ **THE RETURN IS A STRUCT, NOT A FLAT TUPLE.** It used to be fifteen loose returns plus
        /// `liquidationPrice`, i.e. an `AccountPosition`'s numbers with the `marketId` dropped and
        /// the braces removed. That was a SECOND ABI ENCODER for a row this interface already had a
        /// type for, and the duplication is what §7.1.2 exists to fight: two encoders writing the
        /// same seventeen numbers is a field silently dropped from one of them, waiting to happen.
        /// There is now exactly one — `margin_view::AccountPositionRow::to_abi` — reached by both
        /// the single-market and the bulk path.
        ///
        /// So read [`AccountPosition`] — that comment IS the field-by-field contract, including the
        /// rounding mode of every derived value. Nothing is recomputed per surface: every field comes
        /// from ONE `margin_view::MarginInfo` produced by ONE `margin_view::margin_info_of` call,
        /// `liquidationPrice` included.
        ///
        /// `marketId` is now returned, and it is deliberately not "redundant with the argument": it
        /// makes the row SELF-DESCRIBING, so a caller can hand it to the same code that consumes a
        /// `getAccount()` row without carrying the id alongside it — which is the point of the two
        /// paths sharing a type at all.
        ///
        ///   liquidationPrice  the mark price at which this position **IS** liquidatable — for a
        ///                     LONG the GREATEST such price, for a SHORT the LEAST. **Conservative
        ///                     by construction: at this price liquidation FIRES**, and one tick the
        ///                     other way (`+1` for a long, `−1` for a short) it does not. `0` when
        ///                     there is no such price, which covers three states —
        ///                     a FLAT position, a fully-funded LONG (`vQuoteBalance + margin >= 0`,
        ///                     i.e. 1× or lower: as the price falls both notional and requirement
        ///                     go to zero together, so it never becomes liquidatable), and a SHORT
        ///                     so small that its notional cannot outgrow its funding anywhere in
        ///                     the representable price range. `0` is the ABSENCE of a price, not a
        ///                     price of zero; `positionAmt` in the same response separates the flat
        ///                     case.
        ///
        /// ⚠️ WHY THIS IS ON-CHAIN NOW. `liquidationPrice` was being computed OFF-chain against a
        /// flat 1% maintenance ratio, while this chain liquidates off the market's margin-tier
        /// table — `1/6` on today's default tier, sixteen times that rate. Real liquidation
        /// therefore fired far earlier than users were shown. Moving the number here kills the
        /// whole class, because it now comes from the same code path `liquidate()` and the
        /// liquidation sweep enforce.
        ///
        /// ⚠️ IT IS A SEARCH, NOT A FORMULA, AND THAT IS DELIBERATE. `math::calc_liquidation_price`
        /// bisects `math::is_above_maintenance_margin` — the predicate itself — over the mark price,
        /// which is exact by construction and needs no closed form. A closed form would have to
        /// solve a FIXED POINT (the maintenance rate depends on the tier, the tier on the notional,
        /// the notional on the price being solved for), reproduce `maintenance_margin`'s integer
        /// SLICE form (whose subtractive twin is off by one at 53.1% of tier boundaries), and get
        /// both signs of a truncate-toward-zero right. See that function for the monotonicity
        /// argument the bisection rests on, and for what `0` does and does not distinguish.
        /// It costs no storage loads at all — the whole search runs on values this call already
        /// read.
        ///
        /// Reverts if the market does not exist. Note this is UNLIKE `getPosition`, which reports
        /// all-zeros for an unknown market: the decimals and the tier table are required INPUTS here,
        /// so a fabricated zero market would report a full row of plausible zeros.
        ///
        /// ⚠️ **THIS SELECTOR REPLACED `getMarginInfo`, WHICH IS DELETED.** It is a strict superset:
        /// the same fifteen numbers, plus `marketId` and `liquidationPrice`, in a struct instead of a
        /// tuple. A backend rendering SEVERAL markets should use neither — `getAccount().positions[]`
        /// carries this identical row for every market in ONE state, so this selector is for the
        /// genuinely single-market query.
        function getPositionRisk(address user, uint64 marketId) external view returns (
            AccountPosition position
        );

        /// Account-level roll-up of [`AccountPosition`] over an EXPLICIT list of markets.
        ///
        /// **Prefer `getAccount`** unless you specifically want a SUBSET: it takes no list, walks
        /// the per-user market index itself, and its totals are therefore complete. This selector
        /// exists for the "just these markets" query and for paging past MAX_USER_MARKETS.
        ///
        /// The arithmetic is literally shared with `getAccount` — one set of Σ walkers
        /// (`margin_view::account_margin_scalars`), differing only in where the market set comes
        /// from. Duplicate ids are counted ONCE. At most MAX_MARGIN_INFO_MARKETS (64) ids; an
        /// unknown market id reverts. NOTE the engine's own admission gate sums over the per-user
        /// market index, so a SHORT list under-counts `totalOpenOrderInitialMargin` here and
        /// therefore over-reports `availableBalance` relative to what the engine will enforce.
        ///
        ///   totalCrossWalletBalance   the user's perp wallet, SIGNED and unclamped. Binance's
        ///                             `totalCrossWalletBalance` — see the ⚠️ below for why this is
        ///                             NOT Binance's `totalWalletBalance`, which it used to be
        ///                             misnamed after.
        ///   crossMarginBalance        `totalCrossWalletBalance + totalUnrealizedProfit`. NOT a
        ///                             Binance field, and NOT Binance's `totalMarginBalance` (that
        ///                             one is GROSS-based; `getAccount` returns it).
        ///   totalInitialMargin        Σ `initialMargin`         (== the next two, summed).
        ///   totalPositionInitialMargin Σ `positionInitialMargin`.
        ///   totalOpenOrderInitialMargin Σ `openOrderInitialMargin`.
        ///   totalMaintMargin          Σ `maintMargin`.
        ///   totalUnrealizedProfit     Σ `unrealizedProfit`.
        ///   availableBalance          `totalCrossWalletBalance − totalOpenOrderInitialMargin`.
        ///
        /// `totalWalletBalance` is deliberately absent here: `Σ isolatedWallet` over a PARTIAL list
        /// under-counts the silos, so the gross wallet it implies would be a "total" that is not
        /// total. `getAccount` walks the whole index and can name it honestly.
        ///
        /// ⚠️ WHAT `totalCrossWalletBalance` IS NET OF — AND THE NAMING BUG THIS FIXES.
        ///
        /// This field was called `walletBalance` until the index-driven `getAccount` landed. That
        /// was wrong: Binance's `walletBalance` / `totalWalletBalance` is the GROSS wallet, ours is
        /// the CROSS wallet, and the two differ by `Σ isolatedWallet` — a full position's margin.
        /// Anyone lining our field up against Binance's same-named one was off by exactly that.
        /// `marginBalance` → `crossMarginBalance` is the same fix one level up.
        ///
        /// Binance keeps THREE nested balances and derives the innermost on read:
        ///
        /// ```text
        /// walletBalance                                                (gross)
        /// crossWalletBalance = walletBalance      - SUM isolatedWallet (net of positions)
        /// availableBalance   = crossWalletBalance - SUM ooIM           (net of open orders)
        /// ```
        ///
        /// Only the outer two are ledger state there; `SUM ooIM` is never debited from anything,
        /// it is recomputed from the resting book on every read.
        ///
        /// We keep exactly ONE stored balance, and it is the MIDDLE one. `perp_wallet_balance`
        /// has had the POSITION leg physically applied to it (each position's `margin` is debited
        /// when it opens) and the OPEN-ORDER leg NOT — the escrow that used to debit a
        /// `margin_reserved` delta at placement is gone. So:
        ///
        /// ```text
        /// our     totalCrossWalletBalance == Binance totalCrossWalletBalance
        /// our     availableBalance        == Binance availableBalance   (both spendable headroom)
        /// Binance totalWalletBalance      == our cross + SUM isolatedWallet
        ///                                    ^ getAccount returns this, over the whole index
        /// ```
        ///
        /// `availableBalance` here is therefore the REAL headroom — the very quantity the
        /// engine's admission gates compare against — and not, as it was under the escrow, a
        /// figure that subtracted the open-order requirement twice.
        ///
        /// ⚠️ One deliberate departure: **we do NOT truncate `availableBalance` at zero.**
        /// Binance does — it was measured reporting `0.00000000` where the true value was
        /// `-0.00085981`, and the reference doc's verdict is that you therefore cannot use their
        /// field to tell whether an account is under-covered. Ours is `int64` and may go
        /// negative. That is not by itself a distress signal: a mark move alone can push it
        /// there, and like Binance we do not tear resting orders down mid-life for it. What it
        /// does mean is that new risk-INCREASING actions are refused until it recovers.
        ///
        /// A resting order that fills while the wallet cannot fund its margin still FILLS — and the
        /// shortfall lands on the POSITION, not here: `isolatedWallet` is funded
        /// with `min(requirement, cash at hand)` and left short by the rest (model **M1**, measured
        /// on Binance by R11 — `derived-ooim-plan.md` §3a). Consumers must therefore expect
        /// `isolatedWallet < positionInitialMargin` after such a fill, and must not read the gap as
        /// an error: the position's liquidation price is computed from the SHORT margin, which is the
        /// honest one. (Two earlier versions of this comment were wrong: one said such a fill "will be
        /// cancelled at fill time" (pre-escrow-removal behaviour), the next said the shortfall lands
        /// on the cross wallet (model M1′, a conjecture R11 refuted).)
        ///
        /// `totalCrossWalletBalance` is nonetheless still `int64`, because one narrow case remains:
        /// a maker commission the capped opening margin could not absorb — on a pure close, the
        /// whole fee. A NEGATIVE cross wallet is a RECEIVABLE, not protocol bad debt: it is a
        /// negative claim inside the custody identity, it blocks every money-out gate, and it nets
        /// against the user's next deposit. See `types::UserAccount::perp_wallet_balance` for the
        /// derivation, the no-double-count argument, and why absorbing it from the Insurance Fund
        /// would be strictly worse.
        ///
        /// There is no longer any clamped surface to work around: `AccountBalanceChanged` used to
        /// project the cross wallet through a `uint64` floored at 0, which hid a deficit from anyone
        /// watching only the event stream. That field is now `int64 totalCrossWalletBalance` on all
        /// three surfaces (the event, this selector and `getAccount`), so they report the same signed
        /// number. The event carries ONLY the balances, though — the six margin totals below are
        /// REST-only, on this selector and `getAccount`; see the note on `AccountBalanceChanged` for
        /// why a stream cannot keep them fresh.
        ///
        /// Like every selector in this section this is a pure read: it stores nothing and moves no money.
        function getAccountMargin(address user, uint64[] marketIds) external view returns (
            int64  totalCrossWalletBalance,
            int64  crossMarginBalance,
            uint64 totalInitialMargin,
            uint64 totalPositionInitialMargin,
            uint64 totalOpenOrderInitialMargin,
            uint64 totalMaintMargin,
            int64  totalUnrealizedProfit,
            int64  availableBalance
        );

        /// Add isolated margin from the caller's perp wallet to an open position.
        function addPositionMargin(uint64 marketId, uint64 amount) external;
        /// Remove isolated margin from an open position back to the caller's perp wallet.
        function removePositionMargin(uint64 marketId, uint64 amount) external;

        // ── Liquidation ───────────────────────────────────────────────────
        /// Liquidate an under-margined position (anyone can call).
        function liquidate(address user, uint64 marketId) external;

        // ── Insurance Fund ────────────────────────────────────────────────
        /// Deposit USDC from the caller's perp wallet into the global insurance fund. Admin only.
        function depositInsuranceFund(uint64 amount) external;
        /// Withdraw USDC from the insurance fund back to the admin's perp wallet. Admin only.
        function withdrawInsuranceFund(uint64 amount) external;
        /// Query the current insurance fund balance (USDC micro-units).
        function getInsuranceFund() external view returns (uint64 balance);

        // ── Oracle & mark price ───────────────────────────────────────────
        // ── Roles ─────────────────────────────────────────────────────────────
        /// Set the market manager address. Admin only. Zero address revokes the role.
        /// The market manager can call addMarket and updateMarket.
        function setMarketManagerAddress(address manager) external;
        /// Query the current market manager address (zero if not set).
        function getMarketManagerAddress() external view returns (address manager);

        /// Set the authorized oracle address. Admin only. Zero address revokes the role.
        /// The oracle is the only non-admin address allowed to call updateIndexPrice.
        function setOracleAddress(address oracle) external;
        /// Query the current oracle address (zero if not set).
        function getOracleAddress() external view returns (address oracle);

        /// Push a new index price and recompute the mark price using:
        ///   Mark Price = Median(Price1, Price2, ContractPrice)
        ///   Price1 = indexPrice * [1 + fundingRate * (timeUntilNext / fundingInterval)]
        ///   Price2 = indexPrice + MovingAverage30s(midPrice - indexPrice)
        ///   ContractPrice = last on-chain fill price (falls back to indexPrice if no trades yet)
        ///
        /// The 30-second moving average ring buffer is updated each call; each elapsed
        /// second since the last call is filled with the current basis value.
        /// The input timestamp is floored to the market's priceUpdateInterval. If that
        /// floored timestamp is not newer than the stored index timestamp, the update is ignored.
        ///
        /// Callable by admin or the configured oracle address.
        function updateIndexPrice(uint64 marketId, uint64 indexPrice, uint64 timestamp) external;

        /// Query the latest oracle index price and its timestamp for a market.
        function getIndexPrice(uint64 marketId) external view returns (uint64 indexPrice, uint64 lastTimestamp);

        /// Query the current funding state for a market.
        /// interestRate is in getMarket.
        ///
        /// ⚠️ RETURN NAMES CHANGED (metadata only — the selector is unchanged, return names are not
        /// hashed, so a name-bound decoder sees a field vanish rather than a wrong value):
        /// `lastFundingRate` → `fundingRate` and `nextFundingTs` → `nextFundingTime`.
        ///
        /// This is §7.1.2 enforcement, not cosmetics. The market's funding rate was spelled two
        /// ways across the ABI — `fundingRate` on `FundingSettled` and `FundingRateComputed`,
        /// `lastFundingRate` only here — which is the exact failure mode (one quantity, several
        /// names) that §7.1.2 exists to prevent, and the majority spelling also matches the
        /// published `@markPrice` stream's documented "Funding rate". `nextFundingTime` likewise
        /// matches the published "Next funding time". Each quantity now has ONE spelling across
        /// events, selectors and the docs.
        ///
        /// SUFFIX NOTE: `nextFundingTime` sits next to `MarkPriceUpdated.priceWindowTs`, which keeps
        /// `Ts`, and that difference is deliberate — see that event's `priceWindowTs` entry. `Time`
        /// marks a wall-clock instant a consumer publishes; `Ts` marks an internal timestamp-derived
        /// key that must NOT be published as a time. Different quantities, so different names is
        /// correct; §7.1.2 is one name per QUANTITY, not one suffix per type.
        function getFundingState(uint64 marketId) external view returns (int64 fundingRate, uint64 fundingInterval, uint64 nextFundingTime);

        // ── API key management (ed25519 signed orders) ────────────────────
        /// Register an ed25519 public key in slot `keyId` for the caller.
        /// expiry: Unix-second timestamp after which the key is rejected. 0 = never expires.
        /// Overwrites the slot if already occupied.
        /// NOTE: keyId 255 is reserved for the official frontend UI.
        function registerApiKey(uint8 keyId, bytes32 pubkey, uint64 expiry) external;
        /// Remove the key in slot `keyId` for the caller.
        function revokeApiKey(uint8 keyId) external;
        /// Query one key slot for a user. Returns zero pubkey and zero expiry when not set.
        function getApiKey(address user, uint8 keyId) external view returns (bytes32 pubkey, uint64 expiry);
        /// Query all registered key slots for a user.
        function getApiKeys(address user) external view returns (uint8[] keyIds, bytes32[] pubkeys, uint64[] expiries);

        // ── Signed order submission ───────────────────────────────────────
        /// Place an order for `account`, authenticated by ed25519 signature.
        /// Message: "perpdex_v1_order" || account(20) || marketId(8) || side(1)
        ///          || price(8) || quantity(8) || orderType(1) || tif(1) || clientOrderId(16)
        ///          || timestamp(8) || recvWindow(8) || keyId(1)
        /// timestamp: Unix seconds. recvWindow: max age in seconds (capped at 60).
        /// keyId: which API key slot to verify against.
        /// orderId = keccak256(signature) — replay protection via existing order storage.
        function placeOrderSigned(
            address account,
            uint64 marketId,
            uint8 side,
            uint64 price,
            uint64 quantity,
            uint8 orderType,
            uint8 tif,
            bytes16 clientOrderId,
            uint64 timestamp,
            uint64 recvWindow,
            uint8 keyId,
            bytes calldata signature
        ) external returns (bytes32 orderId);

        /// Cancel an order for `account`, authenticated by ed25519 signature.
        /// Message: "perpdex_v1_cancel"(17) || account(20) || orderId(32) || marketId(8) || timestamp(8) || recvWindow(8) || keyId(1)
        /// timestamp: Unix seconds. recvWindow: max age in seconds (capped at 60).
        /// keyId: which API key slot to verify against.
        /// marketId is part of the signed message for ABI compatibility but ignored by the cancel logic.
        /// Replay protection is implicit: a cancelled order cannot be cancelled again.
        function cancelOrderSigned(
            address account,
            bytes32 orderId,
            uint64 marketId,
            uint64 timestamp,
            uint64 recvWindow,
            uint8 keyId,
            bytes calldata signature
        ) external;

        /// Cancel up to MAX_BATCH_CANCEL (256) orders for `account`, authenticated by ONE
        /// ed25519 signature covering the whole batch.
        ///
        /// Message: "perpdex_v1_batch_cancel"(23) || account(20) || keyId(1) || timestamp(8)
        ///          || recvWindow(8) || N(4, big-endian) || N x orderId(32)
        /// N is inside the digest, so the batch's size, content and order cannot be tampered with.
        /// timestamp: Unix seconds. recvWindow: max age in seconds (capped at 60).
        ///
        /// Replay protection is EXPLICIT here (unlike the single-order cancel, whose guard is
        /// implicit in "a cancelled order cannot be cancelled again"): the signature is burned in
        /// the seen-signature set after verification, unconditionally — including a batch in which
        /// every item was rejected. Resubmitting the same signature reverts the whole call.
        ///
        /// Returns the same index-aligned 34-byte-per-item `statuses` blob as batchCancelOrders.
        function batchCancelOrdersSigned(
            address account,
            uint8 keyId,
            uint64 timestamp,
            uint64 recvWindow,
            bytes32[] orderIds,
            bytes calldata signature
        ) external returns (bytes statuses);

        /// Place up to MAX_BATCH_PLACE (64) orders for `account`, authenticated by ONE ed25519
        /// signature covering the whole batch.
        ///
        /// Message: "perpdex_v1_batch_order"(22) || account(20) || keyId(1) || timestamp(8)
        ///          || recvWindow(8) || N(4, big-endian)
        ///          || N x [ marketId(8) || side(1) || price(8) || quantity(8) || orderType(1)
        ///                   || tif(1) || clientOrderId(16) ]        // 43 bytes per item
        /// N is inside the digest, so the batch's size, content and order cannot be tampered with.
        /// timestamp: Unix seconds. recvWindow: max age in seconds (capped at 60).
        ///
        /// orderId derivation differs from the direct path — it does NOT touch the per-user nonce:
        ///   orderId[k] = keccak256(signature || uint32(k) big-endian)
        /// i.e. index-distinct and bound to this one signature. (The single-order `placeOrderSigned`
        /// uses the raw keccak256(signature), which would collide across the N items of a batch.)
        ///
        /// Replay protection is mandatory here (unlike single-order signed placement, where a
        /// REJECTED placement deliberately stays replayable in-window): the signature is burned in
        /// the seen-signature set right after verification, unconditionally — including a batch in
        /// which every item was rejected — because a batch returns Ok and could otherwise be
        /// resubmitted and partly re-execute. Resubmitting reverts the whole call.
        ///
        /// Returns the same index-aligned 34-byte-per-item `statuses` blob as batchPlaceOrders.
        function batchPlaceOrdersSigned(
            address account,
            uint8 keyId,
            uint64 timestamp,
            uint64 recvWindow,
            PlaceItem[] orders,
            bytes calldata signature
        ) external returns (bytes statuses);

        // ── Events ───────────────────────────────────────────────────────
        event AdminInitialized(address indexed admin);
        event AdminTransferred(address indexed previousAdmin, address indexed newAdmin);
        event ApiKeyRegistered(address indexed user, uint8 keyId, bytes32 pubkey, uint64 expiry);
        event ApiKeyRevoked(address indexed user, uint8 keyId);

        // Feeds: /income (TRANSFER type), balance history
        event Deposit(address indexed user, uint256 amount);
        // Feeds: /income (TRANSFER type), balance history
        event Withdraw(address indexed user, uint256 amount);

        // Feeds: internal wallet movement history
        event TransferToPerp(address indexed user, uint64 amount);
        event TransferFromPerp(address indexed user, uint64 amount);
        /// Account BALANCE after-image — our `ACCOUNT_UPDATE`'s `B[]` leg, and nothing else.
        ///
        /// # The payload is exactly what the trigger can guarantee
        ///
        /// This event used to carry ELEVEN fields: the whole account-level margin roll-up
        /// (`totalMarginBalance`, `totalUnrealizedProfit`, `totalInitialMargin`,
        /// `totalPositionInitialMargin`, `totalOpenOrderInitialMargin`, `totalMaintMargin`,
        /// `availableBalance`). Seven of those are gone, because they are things Binance publishes on
        /// **REST `/fapi/v2/account`**, not on the user stream. Binance's measured `ACCOUNT_UPDATE`
        /// payload (R14, mainnet, 2026-08-21, `misc/evidence/binance-run14-user-stream.json`) carries
        /// per-asset balances `B[{a, wb, cw, bc}]` and per-position `P[...]` — **no account-level
        /// margin totals at all.**
        ///
        /// Keeping them here was not merely un-Binance-like, it was INCONSISTENT. We adopted
        /// Binance's TRIGGER (a placement or a cancel publishes nothing — measured, and enforced by
        /// `storage::mark_account_snapshot_dirty`) while keeping the wider payload, and a placement
        /// DOES move `totalOpenOrderInitialMargin` and therefore `availableBalance`. So the event
        /// shipped fields whose freshness its own trigger did not guarantee. Narrowing the payload
        /// fixes that from the payload side: **every field below can only change at a write that
        /// marks the user**, so a stream consumer's copy of these four is never stale.
        ///
        /// `getAccount(address)` is the sole source of the seven that left — and they are LIVE
        /// quantities (they move with the mark while the user does nothing), so a stream could not
        /// have kept them fresh under any trigger short of one that fires on every price update.
        ///
        ///   usdcBalance              spot / withdrawal-layer USDC. NOT part of any total below.
        ///   totalWalletBalance       Binance `wb` — GROSS perp wallet = cross + Σ isolatedWallet.
        ///   totalCrossWalletBalance  Binance `cw` — the STORED `perp_wallet_balance`, verbatim.
        ///
        /// ## All three are STORED SCALARS, so emitting this costs ONE load
        ///
        /// `wb` is not walked. `Σ isolatedWallet` is the incrementally maintained
        /// `UserAccount::total_position_margin` (moved only by `storage::save_position`, from a delta it
        /// gets out of a position read it was already paying for), so `wb = cw + that` is one addition
        /// on the same account blob `usdcBalance` and `cw` come off. The emit path used to walk the
        /// per-user market index and load a position blob per member market — up to 17 reads per
        /// published snapshot, on the fill path. `getAccount` still walks (it loads every position
        /// anyway) and the two are compared in debug builds on every snapshot, so the stored aggregate
        /// cannot drift silently.
        ///
        /// ## `usdcBalance` is a deliberate EXTENSION, not a Binance field
        ///
        /// Binance's futures user stream carries no spot balance at all — the futures wallet is the
        /// only asset it reports. We keep `usdcBalance` anyway because `deposit` / `withdraw` /
        /// `transferToPerp` / `transferFromPerp` move it and every one of them is a trigger site
        /// (they all write the account), so publishing it costs one field on a snapshot that is
        /// already being emitted and saves the consumer a poll it would otherwise have to make on
        /// exactly the events it cares most about. It is an addition to the Binance shape, and a
        /// consumer porting from Binance should expect it rather than look for it in the docs.
        ///
        /// ## `bc` is deliberately ABSENT
        ///
        /// Binance's `B[].bc` is the *balance change excluding PnL and commission* — a DELTA, not a
        /// level. Publishing it would require a per-account baseline held across the transaction:
        /// precisely the `BTreeMap`-of-pre-images + repeated `PublicAccountBalance` construction that
        /// was deleted for cost (see `storage::mark_account_snapshot_dirty`, "Derived from WHICH
        /// WRITE, never from comparing values"). A stream consumer that wants a delta can diff two
        /// consecutive snapshots for the same user, which is strictly more information than `bc`
        /// (`bc` excludes PnL and fees; the diff of two levels includes everything).
        ///
        /// ## Positions are NOT here, and do not need to be
        ///
        /// `PositionChanged` is our `P[]` analogue, and it is assembled into this push by
        /// **ADJACENCY** rather than by ABI fusion. Three rules, and they are a contract:
        ///
        /// 1. every `AccountBalanceChanged` is exactly ONE `ACCOUNT_UPDATE` push;
        /// 2. it is followed by **0..N `PositionChanged` rows**, which are that push's `a.P[]`;
        /// 3. those rows are **CONTIGUOUS**, immediately after this event, and terminated by the
        ///    first non-`PositionChanged` event.
        ///
        /// So an indexer holds the header first and appends rows until the run ends — no correlation
        /// key, no buffering, no second encoding of the same facts in an array here. Two corollaries
        /// follow and are enforced in `debug_assertions` builds by the guard in `crate::events` (which
        /// sits on the emit path itself, so it covers every path, not the ones a test remembered):
        ///
        /// * every `PositionChanged` in a group carries THIS event's `user`;
        /// * there are no ORPHAN `PositionChanged` rows — every one sits inside some group.
        ///
        /// A **0-position group is legitimate**: a deposit, a withdrawal, a transfer, the fee
        /// recipient's cut, and the settled end-state row after a multi-leg liquidation all move a
        /// wallet without moving a position. `a.P[]` is simply empty.
        ///
        /// ⚠️ Rule 3 is why the header comes BEFORE its rows and not after. It used to come after
        /// ("a snapshot closes the rows that caused it"), which reads the same to a human and is
        /// unassemblable by adjacency: the rows would have to be buffered against a header that might
        /// never arrive, and a trailing row from another user would be silently mis-attributed.
        ///
        /// ⚠️ `perpWalletBalance` (`uint64`, floored at 0) is GONE and has been for two revisions. It
        /// is `int64 totalCrossWalletBalance`: the same quantity, SIGNED and UNCLAMPED, and named the
        /// way `getAccount` / `getAccountMargin` name it. The floor was a real blind spot rather than a
        /// cosmetic one — a negative cross wallet IS reachable (a maker close fee the M1-capped
        /// opening margin could not absorb; see `types::UserAccount::perp_wallet_balance` and
        /// `trading::tests::a_maker_close_fee_is_the_only_remaining_way_to_a_negative_wallet`), so an
        /// operator watching only the event stream could not see a deficit accumulate. Nothing on this
        /// event is clamped.
        ///
        /// GRANULARITY: **one per ECONOMIC EVENT**, not one per transaction. Three rules, and a party
        /// falls under exactly one of them per event:
        ///
        /// * a **MAKER** whose resting order is filled: **one per FILL.** A maker order is consumed at
        ///   most once per taker sweep, so this is also one per maker order. The row is emitted inside
        ///   the match, immediately after that fill's `Trade` and immediately BEFORE that fill's
        ///   `PositionChanged` (the `Trade` is outside the group).
        /// * a **TAKER**: **one per ORDER**, after the order has settled (so a batch of K crossing
        ///   items publishes K rows for the initiator, one per item). This INCLUDES the taker leg of
        ///   a liquidation close, which used to be suppressed: a liquidation is legitimately several
        ///   pushes (the close, the clearance fee, each ADL fill), not one settled end-state, and
        ///   suppressing the close's header orphaned the close's position row.
        /// * **funding**, **add/removePositionMargin**, the **liquidation residual close**, and each
        ///   **ADL** fill: **one per economic event**, inline, immediately before that event's own
        ///   position row. A funding settle followed by a margin add is two pushes; one ADL fill is
        ///   two pushes (one for the liquidated user, one for the counterparty).
        /// * **everything else** — deposit / withdraw / transferToPerp / transferFromPerp,
        ///   `setLeverage`, the fee recipient, and the settled end state after a multi-leg
        ///   liquidation — **one per user per transaction**, drained at the end of the call in
        ///   ascending address order, always as a 0-position group.
        ///
        /// A **SELF-TRADE emits BOTH** legs for the same address: the maker-leg row at the fill and
        /// the taker-leg row at the end of the order. The maker leg is deliberately not suppressed —
        /// that would make a maker's own notification conditional on who the counterparty turned out
        /// to be.
        ///
        /// ⚠️ **Consumers must take the LAST row per user**, exactly as they already must for
        /// `PositionChanged`. A user can legitimately appear several times in one transaction, and a
        /// maker row for a user may PRECEDE a later taker row for that same user. Every row is a true
        /// after-image at its own moment; the last is the settled state.
        ///
        /// What this replaced, and why: the unit used to be the transaction, which is the TAKER's
        /// unit. Coalescing a maker's fills into it made a maker's notification cadence a function of
        /// an unrelated party's batching — a 64-item batch filling maker M on items 3, 17 and 40 gave M
        /// one row, at the end of a transaction M never participated in, for three separate economic
        /// events of M's own.
        ///
        /// No row is a half-updated account. It no longer follows from "emitted last" and is instead
        /// established per emit point: the taker's is taken after her margin/fee debit has landed, and
        /// a maker's is the state the match is about to persist for that fill (derived from the match
        /// working copy, whose convergence on the store is asserted in debug builds).
        ///
        /// ⚠️ TRIGGER: a user is included only if the transaction moved that user's WALLET or a
        /// POSITION's stored state. A pure placement and a pure cancel publish NOTHING — and, unlike
        /// before, that is now a complete statement about this payload rather than a caveat on it:
        /// neither of them can move any of the three balances above. The `Σ ooIM` term they DO move
        /// lives on `getAccount` only. Full citation on `storage::mark_account_snapshot_dirty`, which
        /// is where the filter lives; the agreement between this payload and that trigger is pinned by
        /// `trading::tests::account_snapshot_events::placement_and_cancel_cannot_move_any_published_field`.
        ///
        /// ## `reason` — Binance's `a.m`, and WHY it is indexed
        ///
        /// `reason` is `AccountUpdateReason` (perp-core `types::account`): 0 = `ORDER`,
        /// 1 = `FUNDING_FEE`, 2 = `DEPOSIT`, 3 = `WITHDRAW`, 4 = `MARGIN_TRANSFER` (an isolated
        /// position's margin against the wallet — `add`/`removePositionMargin`), 5 =
        /// `ASSET_TRANSFER` (`transferToPerp` / `transferFromPerp`, wallet ↔ wallet), 6 =
        /// `ADJUSTMENT` (protocol-forced, no order: the liquidation residual closed at mark, both
        /// ADL legs, the post-close dust write-off), 7 = `INSURANCE_CLEAR` (the move settles against
        /// the insurance fund: the liquidation clearance fee, `deposit`/`withdrawInsuranceFund`).
        /// **255 = `Multiple`**, which is OURS and not Binance's: the end-of-call drain publishes one
        /// row per user, so two DIFFERENT causes can fold into it, and rather than pick one label and
        /// lie about the other the row says "several". Switch on this field with a default branch.
        ///
        /// Without it the header says only *that* an account moved, and a consumer had to infer the
        /// cause from what surrounds the group — "was there a `Trade` just before it, a
        /// `FundingSettled`, nothing at all". That inference breaks the first time a stream position
        /// changes, and this ABI's history is largely stream positions changing.
        ///
        /// It is **indexed**, spending the third of four topic slots (`user` stays topic[1], so
        /// nothing that reads the subject positionally moves). The reason is that a `reason` in the
        /// data half is only filterable client-side, after the log has already been shipped: an
        /// indexer that wants funding rows only, or an operator watching for `INSURANCE_CLEAR`, can
        /// now express that as an `eth_getLogs` topic filter. It costs nothing here — this
        /// precompile's gas is FLAT PER SELECTOR and never per event or per topic (see the
        /// `SELECTORS` table in `crate::call`), and logs are not part of the perp block commitment.
        event AccountBalanceChanged(
            address indexed user,
            uint8   indexed reason,
            uint256 usdcBalance,
            int64   totalWalletBalance,
            int64   totalCrossWalletBalance
        );
        event UserFeeRatesUpdated(address indexed user, uint64 makerFeeBps, uint64 takerFeeBps);

        // Emitted once per accepted placeOrder / placeOrderSigned call, before any matching.
        // Fires only when validation passes; a reverted tx emits nothing.
        // Feeds: /allOrders (initial record), /openOrders (pending state)
        event OrderPlaced(address indexed user, uint64 indexed marketId, bytes32 indexed orderId, uint8 side, uint64 price, uint64 quantity, uint8 orderType, uint8 tif, bytes16 clientOrderId);

        // Emitted when a limit order rests in the book (after any immediate fills).
        // quantity = the resting quantity (original qty minus any fills that happened first).
        // Feeds: /openOrders (confirm resting), /allOrders (status=NEW/PARTIALLY_FILLED),
        //        and the `@order` websocket stream's `b`/`a` (see `assumingPrice`).
        //
        // `assumingPrice` — this order's FROZEN Assuming Price, the margin basis its resting
        // notional is charged at. `quantity × assumingPrice` (floored to quote units by
        // `calc_value`) IS this order's contribution to the user's `bidNotional` / `askNotional`,
        // i.e. the `@order` stream's `b` / `a` fields, and Σ over the user's resting orders on a
        // side equals `getPositionRisk`'s aggregate for that side exactly.
        //
        // IT IS EMITTED BECAUSE IT IS NOT DERIVABLE ANYWHERE ELSE. For a SELL it is
        // `max(⌈lastTraded × 1.0015⌉, markPrice, limit)` evaluated AT THE INSTANT OF PLACEMENT and
        // then frozen for the order's whole life (R12). An indexer without this field would have to
        // reimplement that rule AND snapshot it at the right moment against a mark and a
        // last-traded price it only sees through other logs. The EVALUATION TIMING of exactly this
        // rule caused a live state leak on val0 (there used to be a second resolution of `T` with
        // `save_last_traded_price` between the two — see THE FREEZE in `trading::rest_in_book`), so
        // it is precisely the kind of rule that must have ONE evaluation, on chain, published.
        //
        // EMITTED UNCONDITIONALLY, on both sides, even though a BUY's Assuming Price is always its
        // own `price` (no markup on the buy side — MEASURED, R11). Suppressing it for buys, or
        // documenting it as "sells only", would make a consumer remember an asymmetry to compute
        // `b` the same way it computes `a`. One rule, `quantity × assumingPrice`, serves both.
        //
        // It is a MARGIN BASIS AND NOTHING ELSE: the book level, the insert position, the fill
        // price and the fee all key on `price`. `assumingPrice >= price` for a sell and
        // `== price` for a buy, so it must NOT be shown to a user as the order's price.
        event OrderRested(address indexed user, uint64 indexed marketId, bytes32 indexed orderId, uint8 side, uint64 price, uint64 quantity, uint8 tif, bytes16 clientOrderId, uint64 assumingPrice);
        // Feeds: /openOrders (remove), /allOrders (status=CANCELED, updateTime)
        //
        // `reason` is `CancelReason` (perp-core `types::order`): 0 = the owner asked (cancelOrder /
        // cancelOrderSigned / batchCancelOrders), 1 = taker margin cover mid-match, 2 = maker fill
        // rejected as opening-into-insolvency, 3 = the owner was liquidated, 4 = the order was
        // stranded out of band at the touch and the mark update expired it.
        //
        // Without it this event was byte-identical for a user cancel and a protocol kill (three
        // indexed fields, no data) and — under delete-on-terminal — `getOrder` answers "not found"
        // for both, so an MM could not tell "my cancel landed" from "my order was taken away".
        // `reason != 0` is the whole test for "I did not ask for this"; treat it as a signal to
        // re-read `getAccount`, since a protocol cancel shrinks Bid/Ask (and therefore raises
        // availableBalance) with no AccountBalanceChanged of its own — see the note on
        // `storage::mark_account_snapshot_dirty`.
        event OrderCancelled(address indexed user, bytes32 indexed orderId, uint64 indexed marketId, uint8 reason);

        // Feeds: /trades, /historicalTrades, /aggTrades, /klines, /ticker/24hr, /myTrades
        // tradeId: global sequential counter for fromId pagination and firstId/lastId in 24hr ticker
        // takerFee / makerFee: USDC micro-units (6-decimal) charged to each side for this fill
        event Trade(uint64 indexed marketId, uint64 tradeId, bytes32 takerOrderId, bytes32 makerOrderId, address taker, address maker, uint64 price, uint64 quantity, uint8 takerSide, uint64 takerFee, uint64 makerFee);

        /// Feeds: /positionRisk (history), /income (REALIZED_PNL), and — since the `@position`
        /// WebSocket stream was removed — the whole of `ACCOUNT_UPDATE.a.P[]`.
        ///
        /// ## This event IS `a.P[]`, and is deliberately SELF-CONTAINED
        ///
        /// The user stream no longer has a position channel: positions ride inside the account
        /// update. So an indexer builds a `P[]` entry from ONE of these and nothing else — it must
        /// not have to join a mark-price feed to fill a field. Mapping, in `P[]` field order:
        ///
        /// ```text
        ///   s    symbol            marketId
        ///   pa   position amount   amount            (signed; positive = long)
        ///   ep   entry price       entryPrice
        ///   bep  breakeven price   breakevenPrice    ⚠️ PLACEHOLDER, always 0 — see below
        ///   cr   cumulative rPnL   cumulativeRealizedPnl  ⚠️ PLACEHOLDER, always 0 — see below
        ///   up   unrealised PnL    unrealizedProfit
        ///   mt   margin type       constant "isolated" — we have no cross mode
        ///   iw   isolated wallet   margin
        ///   ps   position side     constant "BOTH"     — one-way mode only
        ///   ma   margin asset      constant "USDC"     — single-collateral venue
        /// ```
        ///
        /// More than one of these can fire for the same `(user, marketId)` in one transaction (one
        /// per fill, plus one for the funding settle that precedes them). That is not new and needs
        /// no special handling: take the LAST one per `(user, marketId)` — every field is a LEVEL
        /// (an after-image), never a delta, except `realizedPnl` / `closedQuantity`, which are
        /// per-event and must be summed if you want a transaction total.
        ///
        /// ## Field semantics
        ///
        ///   amount, vQuoteBalance, margin, leverage
        ///                     the stored position, verbatim, AFTER this change.
        ///   realizedPnl       gross close PnL FOR THIS EVENT; excludes released margin, fees and
        ///                     funding. Zero on a non-closing update.
        ///   closedQuantity    zero for non-closing position updates.
        ///   entryPrice        `-vQuoteBalance / amount`, in the market's `priceDecimals`
        ///                     fixed-point units — the same scale every other `price` field on this
        ///                     ABI uses, NOT quote micro-units. **`0` when the position is flat**,
        ///                     as `P[].ep` requires. Because `vQuoteBalance` accumulates
        ///                     `-calc_value(fillPrice, qty)` per fill, this is the size-weighted
        ///                     AVERAGE entry over everything still open — after a partial close it
        ///                     is NOT the last fill price. Derived by `math::calc_entry_price`,
        ///                     the algebraic inverse of the `calc_value` the fills used.
        ///   unrealizedProfit  Binance's `positionAmt × (markPrice − entryPrice)` at the market's
        ///                     CURRENT mark, computed as `signedNotional + vQuoteBalance` so the
        ///                     only rounding in it is the truncation already inside
        ///                     `signedNotional`. This is the SAME quantity, under the same
        ///                     lowercase-`r` spelling, that an `AccountPosition` row carries — there is one
        ///                     definition of it in the engine (`margin_view::position_margin_info`)
        ///                     and this field reuses it rather than re-deriving from `entryPrice`,
        ///                     which would round twice.
        ///
        /// ## ⚠️ `cumulativeRealizedPnl` and `breakevenPrice` ARE PLACEHOLDERS. ALWAYS EXACTLY 0.
        ///
        /// **Do not sum them, chart them, or diff them.** They are not "0 because nothing
        /// happened" — they are 0 because the state behind them DOES NOT EXIST. They are carried
        /// so the `P[]` payload shape is stable and adding them later is not a breaking change,
        /// following exactly the precedent the public docs already set for `ACCOUNT_UPDATE.a.m`
        /// ("The field exists so the payload shape is stable, but it is not populated yet — do not
        /// `switch` on it without a default branch"). Treat these the same way: read them as
        /// "unavailable", never as "zero".
        ///
        /// What each one needs before it can be populated:
        ///
        /// * `cumulativeRealizedPnl` (`P[].cr`, "cumulative realised PnL for this symbol") needs a
        ///   **per-position cumulative realised-PnL accumulator** on `PerpPosition`. `PerpPosition`
        ///   has no such field (`amount`, `v_quote_balance`, `margin`, `leverage`,
        ///   `last_funding_index`, `total_buy_qty`, `total_buy_notional`, `total_sell_qty`,
        ///   `total_sell_notional` — that is all of them), and the per-event `realizedPnl` on this
        ///   very event is a DELTA, so nothing on-chain holds the running total. Note it also has
        ///   to survive the position going flat and being reopened, which is precisely why it
        ///   cannot be reconstructed from the current position state.
        /// * `breakevenPrice` (`P[].bep`, "entry adjusted for fees paid") needs **cumulative fees
        ///   paid against the open position**. Also absent: the trading fee is charged out of the
        ///   margin the fill funds and out of the wallet, and no running per-position total is
        ///   kept anywhere. `entryPrice` alone cannot yield it.
        ///
        /// An indexer that wants either number today must accumulate it itself from the event
        /// stream (`realizedPnl` here for `cr`; `Trade.takerFee` / `Trade.makerFee` for `bep`).
        /// When they ARE populated the tests asserting them zero must be updated in the same
        /// change — see `placeholder_position_fields_are_zero`.
        event PositionChanged(address indexed user, uint64 indexed marketId, int64 amount, int64 vQuoteBalance, int64 margin, uint64 leverage, int64 realizedPnl, uint64 closedQuantity, uint64 entryPrice, int64 unrealizedProfit, int64 cumulativeRealizedPnl, uint64 breakevenPrice);
        // Feeds: isolated margin adjustment history. delta > 0 means add margin; delta < 0 means remove margin.
        event PositionMarginAdjusted(address indexed user, uint64 indexed marketId, int64 delta, int64 margin);
        // Feeds: useful for debugging / audit; no direct REST endpoint
        event LeverageChanged(address indexed user, uint64 indexed marketId, uint64 leverage);

        // Feeds: /income (LIQUIDATION_FEE for liquidator)
        event Liquidation(address indexed user, uint64 indexed marketId, address liquidator, int64 amount, uint64 reward, uint64 markPrice);

        // Auto-deleveraging: one per forced trade closing a liquidated insolvent
        // residual (`liquidatedUser`) against an opposite-side holder (`adlUser`) at
        // the liquidated position's bankruptcy `price`. `qty` = deleveraged size.
        event Adl(address indexed liquidatedUser, address indexed adlUser, uint64 indexed marketId, uint64 qty, uint64 price);

        // Feeds: market metadata bootstrap for indexer
        event MarketAdded(uint64 indexed marketId, uint32 baseDecimals, uint32 priceDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, uint64 fundingInterval, int64 interestRate, uint32 liquidationFeeRateBps, uint64 initialMarkPrice, uint32 priceBandBps);
        // Feeds: market risk-table updates for indexer / risk UI.
        // Index-aligned parallel arrays; the full replacement table, not a delta.
        event MarginTiersUpdated(uint64 indexed marketId, uint64[] lowerBounds, uint32[] maxLeverages);
        // Feeds: market metadata updates for indexer
        event MarketUpdated(uint64 indexed marketId, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, bool active, uint64 fundingInterval, int64 interestRate, uint32 liquidationFeeRateBps, uint32 priceBandBps);
        // Feeds: /premiumIndex (mark + index price history), /fundingRate (markPrice field),
        //        and the `@markPrice` websocket stream — this is its ONLY streaming source, and
        //        EVERY instance of it is uniformly valid: there is no seeding/creation special case
        //        in which some fields are meaningless (see the `addMarket` note at the bottom).
        //
        // ── This event ate `IndexPriceUpdated`. ──────────────────────────────────────────────
        // There used to be two events for one thing: `IndexPriceUpdated` (index, mark, price1,
        // price2, ts) fired immediately before `MarkPriceUpdated` (mark, updater) in the SAME
        // `updateIndexPrice` call. `IndexPriceUpdated.markPrice` and `MarkPriceUpdated.price` were
        // the same quantity under two names in two logs — a §7.1.2 collision that also forced every
        // consumer to de-duplicate. `IndexPriceUpdated` is deleted; this event carries its payload.
        //
        // NAME KEPT DELIBERATELY. The payload is entirely new, which is normally the "same name,
        // different meaning" pattern §7.1.2 exists to prevent — but the merge REMOVES a name
        // collision rather than creating one (the mark now has exactly one event and one field name
        // in the streaming path), and the field-list change moves `topic0`, so a stale consumer
        // matches nothing instead of mis-decoding. Renaming would have given a quantity that already
        // had a good name a second one, which is the actual §7.1.2 failure mode.
        //
        // FIELD NAMES: one spelling per quantity across events, selectors and the published stream
        // docs. `fundingRate`/`nextFundingTime` are the majority ABI spelling (`FundingSettled`,
        // `FundingRateComputed`) AND the documented stream names, and `getFundingState`'s returns
        // were RENAMED to them in this change so the split is gone — see that selector. `indexPrice`
        // as `getIndexPrice`, `markPrice` as every other event carrying a mark.
        //
        // * `markPrice`       — the tick-snapped median result. `getMarkPrice(marketId)` after this
        //                       call returns this exact value.
        // * `indexPrice`      — the oracle input for this update. Always a real oracle price:
        //                       `updateIndexPrice` rejects 0, and this event only fires from there.
        // * `fundingRate`     — `FundingState.last_funding_rate` AFTER this call, i.e. the rate in
        //                       force right now, in FUNDING_RATE_ONE (1e6) units. Equal to
        //                       `getFundingState().fundingRate` on the same state, and to the most
        //                       recent `FundingRateComputed.fundingRate`. It is carried HERE, on
        //                       every push, precisely because `FundingRateComputed` only fires at
        //                       epoch boundaries while this fires every `priceUpdateInterval` — a
        //                       consumer no longer has to cache a rate across pushes. 0 only if no
        //                       epoch has closed yet, which is a true statement about the market.
        // * `nextFundingTime` — `FundingState.next_funding_ts` AFTER this call, i.e.
        //                       `getFundingState().nextFundingTime`.
        // * `price1`/`price2` — the two intermediate components of the median, diagnostic only.
        //                       `price2` is `index + MA30s(mid − index)`, i.e. a moving average of
        //                       the BASIS, NOT of the mark. It is therefore **not** the `@markPrice`
        //                       stream's `ap` field; do not wire it there.
        // * `priceWindowTs`   — ⚠️ **NOT A DELIVERY TIME.** The oracle-supplied timestamp FLOORED to
        //                       `priceUpdateInterval`; it identifies WHICH price window this update
        //                       belongs to and is the value stored in `IndexPriceState.timestamp`
        //                       (so it is the same quantity `getIndexPrice` returns as
        //                       `lastTimestamp` — that selector's older name is left alone here).
        //                       The `@markPrice` stream's `E` is BLOCK TIME, which a consumer
        //                       already has from the receipt. Do not wire this to `E`.
        //                       THE `Ts` SUFFIX IS LOAD-BEARING: `Time` in this ABI marks a
        //                       wall-clock instant meant to be published (`nextFundingTime`), `Ts` a
        //                       window key that must not be. Naming this `...Time` would invite
        //                       exactly the `E` mis-wiring the paragraph above forbids.
        // * `updater`         — the admin/oracle address that made the call.
        //
        // NOT CARRIED, deliberately: the stream's `ap` (see `price2` above), `P` (estimated settle
        // price — meaningless for a perpetual) and `E` (block time). Nothing here is a placeholder
        // standing in for them.
        //
        // ⚠️ `addMarket` DOES NOT EMIT THIS, deliberately, and reinstating it is a regression
        // (`risk::tests::add_market_emits_no_price_event`). At market creation there is no index
        // price, no median components and no funding epoch, so five of the nine fields would be 0 —
        // true zeros, but that is not the point: it would make this event's fields only
        // CONDITIONALLY meaningful, and a backend that treats every instance uniformly would
        // publish `i = 0, r = 0, T = 0` once per market and get no error. The initial mark is
        // published by `MarketAdded.initialMarkPrice` instead, which a consumer must read anyway
        // for symbol/decimals/tick. Mark price therefore has two sources — one seeding, one
        // streaming — in exchange for every `MarkPriceUpdated` being valid with no hot-path special
        // case. That is the better trade.
        event MarkPriceUpdated(uint64 indexed marketId, uint64 markPrice, uint64 indexPrice, int64 fundingRate, uint64 nextFundingTime, uint64 price1, uint64 price2, uint64 priceWindowTs, address updater);

        // Feeds: /fundingRate (history), /income (FUNDING_FEE)
        //
        // ⚠️ The old "defined but not yet emitted" note here was STALE. `funding::apply_funding_settlement`
        // emits this on every non-zero settlement, and funding is LAZY: it fires at the first
        // operation that touches the position after a funding epoch boundary (a fill, a margin
        // add/remove, a liquidation), never on a schedule of its own.
        //
        // `amount` is the FULL signed payment (positive = the position received). It is NOT
        // necessarily the change in `P[].iw`: a charge is taken from `pos.margin` down to 0 and the
        // remainder is absorbed by the insurance fund, so applying `amount` to a locally-tracked
        // `iw` over-debits an under-collateralised position. Read the new `iw` off the
        // `PositionChanged` this event is paired with — `apply_funding_settlement` emits one
        // immediately after, valued at the same `markPrice`, for exactly this reason.
        event FundingSettled(uint64 indexed marketId, address indexed user, int64 fundingRate, int64 amount, uint64 markPrice);

        // Feeds: insurance fund balance history
        // delta > 0 = deposit or liquidation surplus credited; delta < 0 = deficit absorbed.
        event InsuranceFundChanged(int64 delta, uint64 newBalance);
        // Emitted when the insurance fund cannot fully cover a deficit.
        // badDebt = the uncovered amount absorbed by the protocol.
        event InsuranceFundDepleted(uint64 indexed marketId, uint64 badDebt);

        // Feeds: role address changes
        event MarketManagerUpdated(address indexed previousManager, address indexed newManager);
        event OracleAddressUpdated(address indexed previousOracle, address indexed newOracle);

        // (`IndexPriceUpdated` used to live here. It was merged into `MarkPriceUpdated` above —
        //  index price, mark price, price1, price2 and the floored window timestamp are all fields
        //  of that one event now. Do not re-add a second price event.)

        // Feeds: /fundingRate — emitted at end of each epoch when a new rate is computed on-chain.
        // avgPremiumIndex: linearly-weighted average PI in FUNDING_RATE_ONE (1e6) units.
        // fundingRate: final rate after Binance formula + clamp.
        // sampleCount: number of updateIndexPrice calls that contributed to this rate.
        event FundingRateComputed(uint64 indexed marketId, int64 fundingRate, int64 avgPremiumIndex, uint64 sampleCount, uint64 timestamp);
    }
}
