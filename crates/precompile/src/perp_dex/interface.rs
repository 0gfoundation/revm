//! Solidity ABI definitions for the PerpDEX precompile.
use alloy_sol_types::sol;

sol! {
    interface IPerpDex {
        /// Deposit USDC into the user's internal account.
        /// Transfers `amount` of USDC from the caller's ERC-20 balance into
        /// the DEX's custody and credits the caller's internal account.
        function deposit(uint256 amount) external;

        /// Withdraw USDC from the user's internal account back to their wallet.
        function withdraw(uint256 amount) external;

        /// Read the current internal state of a user's account.
        function getAccount(address user) external view returns (uint256 usdcBalance);
    }
}
