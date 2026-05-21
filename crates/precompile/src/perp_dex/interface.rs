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
        function getAccount(address user) external view returns (uint256 usdcBalance, uint64 perpWalletBalance);
        /// Set per-user trading fee rates in basis points. Only callable by admin.
        function setUserFeeRates(address user, uint64 makerFeeBps, uint64 takerFeeBps) external;
        /// Query per-user trading fee rates in basis points. Unset users default to zero.
        function getUserFeeRates(address user) external view returns (uint64 makerFeeBps, uint64 takerFeeBps);
        /// Query total trading fees collected for one market.
        function getMarketFeeTotal(uint64 marketId) external view returns (uint64 totalFee);

        // ── Market management (admin only) ─────────────────────────────────
        /// Register a new perpetual market.
        function addMarket(uint64 marketId, uint32 baseDecimals, uint32 priceDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, uint64 fundingInterval, int64 interestRate, uint32 liquidationFeeRateBps) external;
        /// Update mutable market parameters (tick/step/quantity/price limits, active flag, and funding config).
        /// baseDecimals and priceDecimals cannot be changed after creation.
        function updateMarket(uint64 marketId, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, bool active, uint64 fundingInterval, int64 interestRate, uint32 liquidationFeeRateBps) external;
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
        function cancelOrder(bytes32 orderId) external;
        /// Query order details.
        function getOrder(bytes32 orderId) external view returns (
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
        /// Message: "perpdex_v1_cancel" || account(20) || orderId(32) || timestamp(8) || recvWindow(8) || keyId(1)
        /// timestamp: Unix seconds. recvWindow: max age in seconds (capped at 60).
        /// keyId: which API key slot to verify against.
        /// Replay protection is implicit: a cancelled order cannot be cancelled again.
        function cancelOrderSigned(
            address account,
            bytes32 orderId,
            uint64 timestamp,
            uint64 recvWindow,
            uint8 keyId,
            bytes calldata signature
        ) external;

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

        // Feeds: /positionRisk (history), /income (REALIZED_PNL — derived from vQuoteBalance delta)
        event PositionChanged(address indexed user, uint64 indexed marketId, int64 amount, int64 vQuoteBalance, int64 margin, uint64 leverage);
        // Feeds: isolated margin adjustment history. delta > 0 means add margin; delta < 0 means remove margin.
        event PositionMarginAdjusted(address indexed user, uint64 indexed marketId, int64 delta, int64 margin);
        // Feeds: useful for debugging / audit; no direct REST endpoint
        event LeverageChanged(address indexed user, uint64 indexed marketId, uint64 leverage);

        // Feeds: /income (LIQUIDATION_FEE for liquidator)
        event Liquidation(address indexed user, uint64 indexed marketId, address liquidator, int64 amount, uint64 reward, uint64 markPrice);

        // Feeds: market metadata bootstrap for indexer
        event MarketAdded(uint64 indexed marketId, uint32 baseDecimals, uint32 priceDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, uint64 fundingInterval, int64 interestRate, uint32 liquidationFeeRateBps);
        // Feeds: market metadata updates for indexer
        event MarketUpdated(uint64 indexed marketId, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, bool active, uint64 fundingInterval, int64 interestRate, uint32 liquidationFeeRateBps);
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
