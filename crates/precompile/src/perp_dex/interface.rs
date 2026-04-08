//! Solidity ABI definitions for the PerpDEX precompile.
use alloy_sol_types::sol;

sol! {
    interface IPerpDex {
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

        // ── Market management (admin only) ─────────────────────────────────
        /// Register a new perpetual market.
        function addMarket(uint64 marketId, uint32 baseDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity) external;
        /// Update the mark price (used for margin and liquidation).
        function setMarkPrice(uint64 marketId, uint64 price) external;
        /// Read the current mark price for a market.
        function getMarkPrice(uint64 marketId) external view returns (uint64 price);

        // ── Leverage ───────────────────────────────────────────────────────
        /// Set the leverage for the caller's position in a market.
        /// Can only be changed when there are no open positions.
        function setLeverage(uint64 marketId, uint64 leverage) external;

        // ── Trading ────────────────────────────────────────────────────────
        /// Place a limit or market order.
        /// side:      0 = Buy, 1 = Sell
        /// orderType: 0 = Limit, 1 = Market
        /// tif:       0 = GTC, 1 = IOC, 2 = FOK, 3 = PostOnly
        /// Returns a unique order ID.
        function placeOrder(uint64 marketId, uint8 side, uint64 price, uint64 quantity, uint8 orderType, uint8 tif) external returns (bytes32 orderId);
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

        // ── Positions ─────────────────────────────────────────────────────
        /// Query a user's perpetual position in a market.
        function getPosition(address user, uint64 marketId) external view returns (
            int64  amount,
            int64  vQuoteBalance,
            int64  margin,
            uint64 marginReserved,
            uint64 leverage
        );

        // ── Liquidation ───────────────────────────────────────────────────
        /// Liquidate an under-margined position (anyone can call).
        function liquidate(address user, uint64 marketId) external;

        // ── Events ───────────────────────────────────────────────────────
        event Deposit(address indexed user, uint256 amount);
        event Withdraw(address indexed user, uint256 amount);
        event TransferToPerp(address indexed user, uint64 amount);
        event TransferFromPerp(address indexed user, uint64 amount);
        event OrderPlaced(address indexed user, uint64 indexed marketId, bytes32 orderId, uint8 side, uint64 price, uint64 quantity, uint8 orderType);
        event OrderCancelled(address indexed user, bytes32 indexed orderId, uint64 indexed marketId);
        event Trade(uint64 indexed marketId, bytes32 takerOrderId, bytes32 makerOrderId, address taker, address maker, uint64 price, uint64 quantity);
        event PositionChanged(address indexed user, uint64 indexed marketId, int64 amount, int64 vQuoteBalance, int64 margin, uint64 leverage);
        event LeverageChanged(address indexed user, uint64 indexed marketId, uint64 leverage);
        event Liquidation(address indexed user, uint64 indexed marketId, address liquidator, int64 amount, uint64 reward, uint64 markPrice);
        event MarketAdded(uint64 indexed marketId, uint32 baseDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity);
        event MarkPriceUpdated(uint64 indexed marketId, uint64 price, address updater);
    }
}