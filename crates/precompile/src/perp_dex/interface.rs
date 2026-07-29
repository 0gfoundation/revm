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
        /// Query a user's full account balances.
        function getAccount(address user) external view returns (uint256 usdcBalance, uint64 availablePerpBalance);
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

        // ── Trading ────────────────────────────────────────────────────────
        /// Place a limit or market order.
        /// side:      0 = Buy, 1 = Sell
        /// orderType: 0 = Limit, 1 = Market
        /// tif:       0 = GTC, 1 = IOC, 2 = FOK, 3 = PostOnly
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
            bytes16 clientOrderId;
        }
        /// Place up to MAX_BATCH_PLACE (64) orders in one call, on behalf of the caller.
        ///
        /// Atomicity is **abort-forward**, not all-or-nothing: items run in strict calldata order and
        /// a per-item genuine reject (unknown/inactive market, bad side/orderType/tif, qty or price
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
        function getPosition(address user, uint64 marketId) external view returns (
            int64  amount,
            int64  vQuoteBalance,
            int64  margin,
            uint64 marginReserved,
            uint64 feeReserved,
            uint64 leverage
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
        ///   Price1 = indexPrice * [1 + lastFundingRate * (timeUntilNext / fundingInterval)]
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
        function getFundingState(uint64 marketId) external view returns (int64 lastFundingRate, uint64 fundingInterval, uint64 nextFundingTs);

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
        /// Final public balance after-image, emitted once per changed account per successful call.
        event AccountBalanceChanged(address indexed user, uint256 usdcBalance, uint64 availablePerpBalance);
        event UserFeeRatesUpdated(address indexed user, uint64 makerFeeBps, uint64 takerFeeBps);

        // Emitted once per accepted placeOrder / placeOrderSigned call, before any matching.
        // Fires only when validation passes; a reverted tx emits nothing.
        // Feeds: /allOrders (initial record), /openOrders (pending state)
        event OrderPlaced(address indexed user, uint64 indexed marketId, bytes32 indexed orderId, uint8 side, uint64 price, uint64 quantity, uint8 orderType, uint8 tif, bytes16 clientOrderId);

        // Emitted when a limit order rests in the book (after any immediate fills).
        // quantity = the resting quantity (original qty minus any fills that happened first).
        // Feeds: /openOrders (confirm resting), /allOrders (status=NEW/PARTIALLY_FILLED)
        event OrderRested(address indexed user, uint64 indexed marketId, bytes32 indexed orderId, uint8 side, uint64 price, uint64 quantity, uint8 tif, bytes16 clientOrderId);
        // Feeds: /openOrders (remove), /allOrders (status=CANCELED, updateTime)
        event OrderCancelled(address indexed user, bytes32 indexed orderId, uint64 indexed marketId);

        // Feeds: /trades, /historicalTrades, /aggTrades, /klines, /ticker/24hr, /myTrades
        // tradeId: global sequential counter for fromId pagination and firstId/lastId in 24hr ticker
        // takerFee / makerFee: USDC micro-units (6-decimal) charged to each side for this fill
        event Trade(uint64 indexed marketId, uint64 tradeId, bytes32 takerOrderId, bytes32 makerOrderId, address taker, address maker, uint64 price, uint64 quantity, uint8 takerSide, uint64 takerFee, uint64 makerFee);

        // Feeds: /positionRisk (history), /income (REALIZED_PNL)
        // realizedPnl is gross close PnL and excludes released margin, fees, and funding.
        // closedQuantity is zero for non-closing position updates.
        event PositionChanged(address indexed user, uint64 indexed marketId, int64 amount, int64 vQuoteBalance, int64 margin, uint64 leverage, int64 realizedPnl, uint64 closedQuantity);
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
        // Feeds: market metadata updates for indexer
        event MarketUpdated(uint64 indexed marketId, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, bool active, uint64 fundingInterval, int64 interestRate, uint32 liquidationFeeRateBps, uint32 priceBandBps);
        // Feeds: /premiumIndex (mark price history), /fundingRate (markPrice field)
        event MarkPriceUpdated(uint64 indexed marketId, uint64 price, address updater);

        // Feeds: /fundingRate (history), /income (FUNDING_FEE)
        // NOTE: defined but not yet emitted — will be wired when funding settlement is implemented.
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

        // Feeds: /premiumIndex (index price history), /markPrice websocket
        // price1/price2 are the two intermediate components; markPrice is the median result.
        event IndexPriceUpdated(uint64 indexed marketId, uint64 indexPrice, uint64 markPrice, uint64 price1, uint64 price2, uint64 timestamp);

        // Feeds: /fundingRate — emitted at end of each epoch when a new rate is computed on-chain.
        // avgPremiumIndex: linearly-weighted average PI in FUNDING_RATE_ONE (1e6) units.
        // fundingRate: final rate after Binance formula + clamp.
        // sampleCount: number of updateIndexPrice calls that contributed to this rate.
        event FundingRateComputed(uint64 indexed marketId, int64 fundingRate, int64 avgPremiumIndex, uint64 sampleCount, uint64 timestamp);
    }
}
