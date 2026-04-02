# PerpDEX On-Chain Precompile — Weekly Progress Report

---

## Module Structure

```
crates/precompile/src/perp_dex/
│
├── mod.rs              ✅  Selector routing & entry point
├── interface.rs        ✅  Solidity ABI  (sol! macro)
├── errors.rs           ✅  perp_err() unified error factory
│
├── types/
│   ├── account.rs      ✅  UserAccount  (msgpack-serialised)
│   ├── order.rs        🔲  Placeholder  (Step 2)
│   └── position.rs     🔲  Placeholder  (Step 2)
│
├── storage/
│   ├── keys.rs         ✅  Storage key derivation
│   └── mod.rs          ✅  Account store + ERC-20 direct r/w
│
├── account/
│   └── deposit_withdraw.rs  ✅  Core logic + unit tests (9/9)
│
├── trading/mod.rs      🔲  Placeholder  (Step 2 — order book)
└── risk/mod.rs         🔲  Placeholder  (Step 3 — margin / liquidation)
```

- ✅ Implemented
- 🔲 Placeholder / Future

---

## Implemented Functions (Step 1)

| # | Selector | Gas | Type | Status |
|---|----------|-----|------|--------|
| 1 | `deposit(uint256 amount)` | 50,000 | Write | ✅ Done |
| 2 | `withdraw(uint256 amount)` | 50,000 | Write | ✅ Done |
| 3 | `getAccount(address user)` | 5,000 | Read | ✅ Done |

---

## Unit Tests — 9 / 9 Passing ✅

| Group | Test Name | Scenario |
|-------|-----------|----------|
| `deposit` | `deposit_moves_usdc_to_internal_account` | Normal deposit → internal balance correct |
| `deposit` | `deposit_partial_then_check_remainder` | Partial deposit → correct balance delta |
| `deposit` | `deposit_rejects_zero_amount` | Rejects `amount = 0` |
| `deposit` | `deposit_rejects_insufficient_erc20` | Rejects when ERC-20 balance too low |
| `withdraw` | `withdraw_returns_usdc_to_wallet` | Full withdraw after deposit → balance back to 0 |
| `withdraw` | `withdraw_partial_leaves_remainder` | Partial withdraw → remaining balance correct |
| `withdraw` | `withdraw_rejects_zero_amount` | Rejects `amount = 0` |
| `withdraw` | `withdraw_rejects_overdraft` | Rejects overdraft on internal balance |
| `getAccount` | `get_account_returns_zero_for_new_user` | New account returns 0 — no storage written |

---

## Roadmap

```
●  Step 1   Account System                        ✅  COMPLETE (this week)
│           deposit / withdraw / getAccount
│           9 unit tests — all passing
│
●  Step 2   Trading Engine                        🔲  Next
│           Order book & matching logic
│
●  Step 3   Risk Management                       🔲  Planned
│           Margin / Liquidation / Funding Rate
│           Market management
│
●  Step 4   Integration & Validation              🔲  Future
            End-to-end tests with full EVM context
            Solidity contract interaction tests
```

---

