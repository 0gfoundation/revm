# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

REVM is a highly efficient Rust implementation of the Ethereum Virtual Machine (EVM). It serves both as:
1. A standard EVM for executing Ethereum transactions
2. A framework for building custom EVM variants (like Optimism's op-revm)

The project is used by major Ethereum infrastructure including Reth, Foundry, Hardhat, Optimism, Scroll, and many zkVMs.

## 0G PerpDEX Development Posture (READ FIRST)

This is the **0gfoundation fork** of revm. It hosts the **PerpDEX engine** (address `0x…1003`) and its **off-trie PerpState + chained block commitment**, consumed by the 0G chain (0g-reth). Since the `perp-engine-extraction` branch the perp code is split into three layers:

- `crates/perp-core` — state core (types, math, keys, msgpack codec, `TypedPerpStore`, `compute_block_commitment`); error type `PerpError`. No EVM deps beyond `revm-context-interface` (the journal seam types stay there).
- `crates/perp-engine` — the matching engine (trading/settlement/liquidation, risk, account, funding, batch, sol! ABI, storage access layer, and the call shell: selector table/gas/depth gate/revert encoding). Generic over `host::PerpHost`; a **blanket impl makes every `CTX: ContextTr` a host**, and `InMemoryHost` runs the engine standalone (perf benches drive it without building the EVM). `end_block()` + `compute_block_commitment` reproduce the block lifecycle.
- `crates/precompile/src/perp_dex/` — a thin shell: `PrecompileResult` adapter over `perp_engine::run_perp_dex_call`, the block-end commitment anchor (`storage::finalize_block_commitment`, path unchanged for alloy-evm), `From<PerpError> for PrecompileError`, and facade re-exports at all historical `perp_dex::*` paths (so external callers and the bench harness are unaffected).

When touching engine logic, edit `perp-engine`/`perp-core`; the golden commitment test (`commitment_golden_scenario`, now in `perp-engine`) still pins byte-identity.

**The PerpDEX / off-trie-state / commitment layer is PRE-PRODUCTION. There is no mainnet, no users, no historical data, and the chain is wiped and restarted at will.** Therefore, for changes scoped to the PerpDEX precompile, its off-trie state, its serialization, and its block commitment:

- **Do NOT weigh forward/backward compatibility, migrations, or "this changes consensus state" as a blocker.** A devnet wipe accompanies any such change. Decide PURELY on implementation ROI (does the win justify the code/complexity?), NOT on SAFE-vs-CHAIN.
- A change to the **commitment scheme/serialization** (the hashing format itself, e.g. the per-call→per-block transition) is a routine step: bump `BLOCK_COMMITMENT_VERSION`, re-pin the golden commitment test, wipe. It is cheap.
- A change to **execution rules** that alters the per-block write-set or state (e.g. skipping a redundant store, a new margin formula like the flip-aware reservation) only needs the golden commitment test re-pinned (run `commitment_golden_scenario`, copy the printed value). The `BusinessSnapshot` in that test pins business semantics independently — keep it unchanged unless the business behavior is *intended* to change.
- **TRIPWIRE:** if you ever find yourself recommending "leave it / don't optimize" *because* a change "would alter the commitment / require a version bump / break compat" — STOP. That reasoning is invalid here. Re-decide on ROI alone. (The only thing that still matters: correctness, determinism across nodes running the SAME code, and EVM semantics for genuine Ethereum behavior.)

This window closes at mainnet; until then, optimize aggressively.

## Build and Development Commands

### Essential Commands
```bash
# Build the project
cargo build
cargo build --release

# Run all tests
cargo nexttest run --workspace

# Lint and format
cargo clippy --workspace --all-targets --all-features
cargo fmt --all

# Check no_std compatibility
cargo check --target riscv32imac-unknown-none-elf --no-default-features

# Run Ethereum state tests
cargo run -p revme statetest legacytests/Cancun/GeneralStateTests
```

### Test Scripts
```bash
# Download and run ethereum tests
./scripts/run-tests.sh

# Clean test fixtures and re-run
./scripts/run-tests.sh clean

# Run with specific profile
./scripts/run-tests.sh release
```

## Architecture

The workspace consists of these core crates:

- **revm**: Main crate that re-exports all others
- **revm-primitives**: Constants, primitive types, and core data structures
- **revm-interpreter**: EVM opcode implementations and execution engine
- **revm-context**: Execution context, environment, and journaled state
- **revm-handler**: Execution flow control and call frame management
- **revm-database**: State database traits and implementations
- **revm-precompile**: Ethereum precompiled contracts
- **revm-inspector**: Tracing and debugging framework
- **op-revm**: Example of custom EVM variant (Optimism)

### Key Design Patterns

1. **Trait-based Architecture**: Core functionality is defined through traits, allowing custom implementations
2. **Handler Pattern**: Execution flow is controlled through customizable handlers
3. **no_std Support**: All core crates support no_std environments
4. **Feature Flags**: Extensive use of feature flags for optional functionality

### Important Interfaces

1. **Database Trait** (`revm-database`): Defines how state is accessed
2. **Inspector Trait** (`revm-inspector`): Hooks for transaction tracing
3. **Handler Interface** (`revm-handler`): Customizable execution logic
4. **Context** (`revm-context`): Manages execution state and environment

## Current Development Context

When working on the `frame_stack` branch, note that significant refactoring is happening around:
- Frame and FrameData structures (moved from handler to context)
- Execution loop simplification
- Inspector trait cleanup

## Testing Strategy

1. Unit tests in each crate
2. Integration tests using Ethereum official test suite
3. Example projects demonstrating features
4. Benchmarking with CodSpeed

When adding new features:
- Ensure no_std compatibility
- Add appropriate feature flags
- Include tests for new functionality
- Update relevant examples if needed