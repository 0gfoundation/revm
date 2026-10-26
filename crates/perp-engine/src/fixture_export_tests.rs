//! **Real-engine event fixtures for the perpdex-perf correctness harness** (test-only exporter).
//!
//! There is no chain to validate the harness against, so this module produces what a chain would
//! have handed it. Every scenario runs ONLY through the call entry point ([`run_perp_dex_call`]) on
//! an [`InMemoryHost`] whose blocks are ended explicitly, and every call is recorded the way its
//! receipt reports it: block number and timestamp, transaction index, sender, the precompile
//! address, selector and calldata, success, gas, the revert string, the return data, and the logs
//! exactly as emitted (a reverted call keeps none, as the EVM frame of a reverted call does not).
//! At chosen block ends the fixture also carries the raw return data of every getter the harness
//! reads (`eth_call` at that block), for every actor and market, plus the ERC-20 `balanceOf`s.
//!
//! # Scenarios
//!
//! * `isolated_golden` — the actions of `trading::tests::golden` (`commitment_golden_scenario`),
//!   one block at timestamp 1 as there. Its block commitment must equal the pinned
//!   `GOLDEN_COMMITMENT`, which proves the replay is that scenario write for write.
//! * `cross_golden` — the actions of `cross_golden_tests`. That scenario moves the block clock
//!   inside ONE block; here every clock move starts a new block (the chain's shape). The same
//!   actions run in one block first and must reproduce `CROSS_GOLDEN_COMMITMENT`; the split run
//!   must then end with every getter returning the same bytes (block boundaries are
//!   business-neutral).
//! * `mixed_random` — a seeded random trace: three markets shaped like the devnet's BTC / ETH / SOL
//!   (ids 2 / 4 / 6, all on the harness's 3 / 2 decimals), CROSSED and ISOLATED traders, liquidity
//!   providers, sub-accounts, and every route the harness replays, plus scenes that guarantee the
//!   cross episode, an isolated liquidation, ADL, funding epochs and the negative-wallet clearance;
//!   coverage is asserted.
//! * `isolated_random` — the same generator with every trading actor `setMarginType(ISOLATED)` on
//!   every market before it trades: the margin shape the harness's existing scenarios have.
//! * `isolated_random_m2` — `isolated_random` on market 2 alone, the shape the harness's
//!   single-market runs (`MARKET_ID` = 2) model end to end.
//! * `cross_scenes` — deterministic scenes a random trace reaches rarely or never: a lazily-deleted
//!   id in a level's FIFO under a checkpoint; a CROSSED loser ADL-ed against an under-water CROSSED
//!   winner (the fund covering it), with the loser's capped fee and the winner's negative-wallet
//!   clearance in one call — in both address orders; a CROSSED maker refused on a cleared wallet;
//!   a maker refused at its leverage tier.
//! * `cross_episodes` — the episode's own decisions: a stale leg left open (Q10) and the flag that
//!   stays, -2023, a flagged maker's reduce-only fill (R1) ending the flag, an isolated liquidation
//!   next to a cross wallet, and three legs closed in one call in the documented order.
//!
//! Every checkpoint also reads `getAccountMargin` over no market (the STORED wallet, which
//! `AccountBalanceChanged` carries) and over each market an actor holds a position in (that leg's
//! unsettled cross funding on top) — what a harness seeding mid-trace starts from.
//!
//! # Regenerate
//!
//! ```text
//! cd /home/harry/umi && PERP_FIXTURE_OUT=/home/harry/perpdex-perf/test/fixtures/engine \
//!   PERP_FIXTURE_UMI_COMMIT=$(git rev-parse HEAD) \
//!   cargo test -p perp-engine --lib fixture_export -- --ignored --test-threads=1
//! ```
//!
//! The tests are `#[ignore]`d — they are a tool, not a gate on the engine. Without
//! `PERP_FIXTURE_OUT` they still build every fixture and run its checks, and write nothing.
//!
//! # What stands in for the chain
//!
//! * ERC-20 USDC is the host's external balance map. The precompile writes the token's balance
//!   slot directly, so a deposit or withdraw emits no `Transfer` log on chain either. Genesis
//!   balances (`genesis.erc20Balances`) stand for a MockUSDC mint before block 1, and the
//!   `balanceOf` reads are answered from the same map, encoded as the token returns them.
//! * `gas` is the precompile's charge (`PerpOutput.gas_used`). A receipt's `gasUsed` adds the
//!   intrinsic 21_000 and the calldata cost on top. Every call runs with a 30M gas limit.
//! * There are no transaction hashes, nonces or native balances. `perpCommitment` per block is the
//!   chained block commitment (`compute_block_commitment`, genesis 0), as the block executor folds
//!   it onto 0x1003.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use alloy_sol_types::{SolCall, SolError, SolEvent};
use ed25519_dalek::{Signer, SigningKey};
use primitives::{address, Address, Bytes, FixedBytes, Log, B256, U256};

use crate::host::{InMemoryHost, PerpHost};
use crate::interface::IPerpDex as I;
use crate::interface::IPerpDex::{
    addMarketCall, addPositionMarginCall, asSubAccountCall, batchCancelOrdersCall,
    batchPlaceOrdersCall, cancelOrderCall, cancelOrderSignedCall, createSubAccountCall,
    depositCall, depositInsuranceFundCall, drainDeferredWorkCall, getAccountCall,
    getAccountMarginCall, getAdminCall, getAlgoOrderCall, getApiKeysCall, getBookLevelCall,
    getBookPricesCall, getFundingStateCall, getIndexPriceCall, getInsuranceFundCall,
    getMarginTiersCall, getMarkPriceCall, getMarketCall, getMarketFeeTotalCall, getMasterCall,
    getMaxMakerRebateCall, getOpenAlgoOrdersCall, getOpenOrdersCall, getOrderCall,
    getPositionCall, getPositionRiskCall, getSubAccountsCall, getSymbolConfigCall,
    getUserFeeRatesCall, initAdminCall, internalTransferCall, liquidateCall, placeAlgoOrderCall,
    placeOrderCall, placeOrderSignedCall, registerApiKeyCall, removePositionMarginCall,
    revokeApiKeyCall, setLeverageCall, setLeverageSignedCall, setMarginTiersCall,
    setMarginTypeCall, setMarketManagerAddressCall, setMaxMakerRebateCall, setOracleAddressCall,
    setUserFeeRatesCall, transferAdminCall, transferFromPerpCall, transferToPerpCall,
    updateIndexPriceCall, updateMarketCall, withdrawCall, withdrawInsuranceFundCall, PlaceItem,
};
use crate::{run_perp_dex_call, PERP_DEX_ADDRESS, USDC_ADDRESS};

/// Gas limit of every recorded call and read.
const GAS: u64 = 30_000_000;
/// Hard ceiling on one fixture file (the harness keeps them in git).
const MAX_FIXTURE_BYTES: usize = 5_000_000;
const SCHEMA: &str = "perpdex-engine-fixture/1";

type Id = [u8; 32];

// ── Name tables (selector → function, topic0 → event) ──────────────────────────────────────────

/// `"name(types…)"` → `"name"`.
fn sig_name(sig: &'static str) -> &'static str {
    &sig[..sig.find('(').unwrap_or(sig.len())]
}

macro_rules! call_table {
    ($($c:ident),* $(,)?) => {
        /// Every function of `IPerpDex`, by selector.
        fn call_table() -> Vec<([u8; 4], &'static str)> {
            vec![$((I::$c::SELECTOR, sig_name(<I::$c as SolCall>::SIGNATURE))),*]
        }
    };
}

call_table!(
    initAdminCall, transferAdminCall, getAdminCall, depositCall, withdrawCall, transferToPerpCall,
    transferFromPerpCall, transferToPerpSignedCall, transferFromPerpSignedCall, getAccountCall,
    setUserFeeRatesCall, getUserFeeRatesCall, getMarketFeeTotalCall, setMaxMakerRebateCall,
    getMaxMakerRebateCall, createSubAccountCall, asSubAccountCall, internalTransferCall,
    getSubAccountsCall, getMasterCall, addMarketCall, updateMarketCall, getMarkPriceCall,
    getMarketCall, drainDeferredWorkCall, placeAlgoOrderCall, cancelAlgoOrderCall,
    getAlgoOrderCall, getOpenAlgoOrdersCall, placeAlgoOrderSignedCall, cancelAlgoOrderSignedCall,
    getAveragePremiumIndexCall, setMarginTiersCall, setBasisModeCall, getMarginTiersCall,
    setLeverageCall, setLeverageSignedCall, getSymbolConfigCall, setMarginTypeCall,
    setMarginTypeSignedCall, placeOrderCall, cancelOrderCall, batchCancelOrdersCall,
    batchPlaceOrdersCall, getOrderCall, getOpenOrdersCall, getBookPricesCall, getBookLevelCall,
    getPositionCall, getPositionRiskCall, getAccountMarginCall, addPositionMarginCall,
    removePositionMarginCall, liquidateCall, depositInsuranceFundCall, withdrawInsuranceFundCall,
    getInsuranceFundCall, setMarketManagerAddressCall, getMarketManagerAddressCall,
    setOracleAddressCall, getOracleAddressCall, updateIndexPriceCall,
    updateIndexPriceFromBookCall, getIndexPriceCall, getIndexModeCall, getFundingStateCall,
    registerApiKeyCall, revokeApiKeyCall, getApiKeyCall, getApiKeysCall, placeOrderSignedCall,
    cancelOrderSignedCall, batchCancelOrdersSignedCall, batchPlaceOrdersSignedCall,
);

macro_rules! event_table {
    ($($e:ident),* $(,)?) => {
        /// Every event of `IPerpDex`, by topic0.
        fn event_table() -> Vec<(B256, &'static str)> {
            vec![$((I::$e::SIGNATURE_HASH, sig_name(<I::$e as SolEvent>::SIGNATURE))),*]
        }
    };
}

event_table!(
    AdminInitialized, AdminTransferred, ApiKeyRegistered, ApiKeyRevoked, Deposit, Withdraw,
    TransferToPerp, TransferFromPerp, SubAccountCreated, InternalTransfer, AccountBalanceChanged,
    UserFeeRatesUpdated, MaxMakerRebateUpdated, OrderPlaced, AlgoOrderPlaced, AlgoOrderTriggered,
    AlgoOrderActivated, AlgoOrderCancelled, OrderRested, OrderCancelled, OrderExpired, Trade,
    PositionChanged, PositionMarginAdjusted, LeverageChanged, MarginTypeChanged, Liquidation,
    CrossLiquidationStarted, CrossLiquidationEnded, Adl, MarketAdded, MarginTiersUpdated,
    MarketUpdated, MarkPriceUpdated, FundingSettled, InsuranceFundChanged, InsuranceFundDepleted,
    MarketManagerUpdated, OracleAddressUpdated, FundingRateComputed,
);

fn call_name(sel: [u8; 4]) -> &'static str {
    call_table()
        .into_iter()
        .find(|(s, _)| *s == sel)
        .map(|(_, n)| n)
        .unwrap_or_else(|| panic!("selector 0x{} is not in IPerpDex", primitives::hex::encode(sel)))
}

fn event_name(topic0: &B256) -> &'static str {
    event_table()
        .into_iter()
        .find(|(t, _)| t == topic0)
        .map(|(_, n)| n)
        .unwrap_or_else(|| panic!("log topic0 {topic0} is not an IPerpDex event"))
}

/// The tables must list the whole interface — a new function or event fails here, not silently.
#[test]
fn fixture_export_name_tables_cover_the_interface() {
    let calls = call_table();
    assert_eq!(calls.len(), I::IPerpDexCalls::SELECTORS.len(), "call_table! is incomplete");
    for sel in I::IPerpDexCalls::SELECTORS {
        assert!(calls.iter().any(|(s, _)| s == sel), "call_table! misses a selector");
    }
    let events = event_table();
    assert_eq!(events.len(), I::IPerpDexEvents::SELECTORS.len(), "event_table! is incomplete");
    for t in I::IPerpDexEvents::SELECTORS {
        assert!(events.iter().any(|(e, _)| e.0 == *t), "event_table! misses an event");
    }
}

// ── JSON (hand-rolled: the engine has no serde_json) ───────────────────────────────────────────

fn hex(b: &[u8]) -> String {
    format!("0x{}", primitives::hex::encode(b))
}

fn haddr(a: Address) -> String {
    hex(a.as_slice())
}

/// A JSON string literal.
fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A JSON array of string literals.
fn jarr(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|s| js(s)).collect::<Vec<_>>().join(","))
}

fn revert_reason(bytes: &[u8]) -> String {
    alloy_sol_types::Revert::abi_decode(bytes)
        .map(|r| r.reason)
        .unwrap_or_else(|_| format!("<undecodable revert {}>", hex(bytes)))
}

// ── The recorder ───────────────────────────────────────────────────────────────────────────────

struct CallRec {
    block: u64,
    timestamp: u64,
    tx_index: u32,
    from: Address,
    input: Vec<u8>,
    success: bool,
    gas: u64,
    ret: Vec<u8>,
    revert: Option<String>,
    /// `(block-scoped logIndex, log)`.
    logs: Vec<(u32, Log)>,
}

struct BlockRec {
    number: u64,
    timestamp: u64,
    first_call: usize,
    calls: usize,
    logs: u32,
    commitment: U256,
}

struct ReadRec {
    to: Address,
    name: &'static str,
    /// A JSON array literal.
    args: String,
    input: Vec<u8>,
    ret: Vec<u8>,
    reverted: bool,
}

struct Checkpoint {
    label: String,
    after_block: u64,
    after_call: usize,
    reads: Vec<ReadRec>,
}

/// One chain, recorded: the host plus every call, block and checkpoint read so far.
struct Recorder {
    h: InMemoryHost,
    /// The number of the OPEN block (the one the next call lands in).
    block: u64,
    open_calls: usize,
    log_index: u32,
    commitment: U256,
    calls: Vec<CallRec>,
    blocks: Vec<BlockRec>,
    checkpoints: Vec<Checkpoint>,
    actors: Vec<(Address, String)>,
    markets: Vec<u64>,
    genesis: Vec<(Address, U256)>,
    /// Return data of the last recorded call.
    last_ret: Vec<u8>,
}

impl Recorder {
    fn new(now: u64, chain_id: u64) -> Recorder {
        Recorder {
            h: InMemoryHost::new(now).with_chain_id(chain_id),
            block: 1,
            open_calls: 0,
            log_index: 0,
            commitment: U256::ZERO,
            calls: Vec::new(),
            blocks: Vec::new(),
            checkpoints: Vec::new(),
            actors: Vec::new(),
            markets: Vec::new(),
            genesis: Vec::new(),
            last_ret: Vec::new(),
        }
    }

    fn actor(&mut self, a: Address, label: &str) {
        if !self.actors.iter().any(|(x, _)| *x == a) {
            self.actors.push((a, label.to_string()));
        }
    }

    /// A genesis ERC-20 balance (a MockUSDC mint before block 1).
    fn mint(&mut self, a: Address, amount: u64) {
        assert!(self.calls.is_empty(), "ERC-20 balances are minted at genesis only");
        let held = self.h.external_balance(USDC_ADDRESS, a).unwrap();
        let v = held + U256::from(amount);
        self.h.set_external_balance(USDC_ADDRESS, a, v).unwrap();
        match self.genesis.iter_mut().find(|(x, _)| *x == a) {
            Some(e) => e.1 = v,
            None => self.genesis.push((a, v)),
        }
    }

    /// One transaction to 0x…1003, recorded whichever way it goes.
    fn send(&mut self, from: Address, input: &[u8]) -> Result<Vec<Log>, String> {
        let _ = call_name(input[..4].try_into().unwrap());
        let n = self.h.logs.len();
        let out = run_perp_dex_call(input, GAS, from, U256::ZERO, false, &mut self.h)
            .unwrap_or_else(|e| panic!("hard failure (not a revert) from {from}: {e:?}"));
        let logs = self.h.logs.split_off(n);
        let (revert, logs) = if out.reverted {
            // A reverted frame keeps no logs on chain. The engine emits none before a reject
            // (validate-then-apply), which this pins.
            assert!(logs.is_empty(), "a reverted call emitted logs");
            (Some(revert_reason(&out.bytes)), Vec::new())
        } else {
            (None, logs)
        };
        for l in &logs {
            assert_eq!(l.address, PERP_DEX_ADDRESS, "a log from another address");
            let _ = event_name(l.data.topics().first().expect("an anonymous log"));
        }
        let indexed: Vec<(u32, Log)> = logs
            .iter()
            .cloned()
            .map(|l| {
                let i = self.log_index;
                self.log_index += 1;
                (i, l)
            })
            .collect();
        self.calls.push(CallRec {
            block: self.block,
            timestamp: self.h.now,
            tx_index: self.open_calls as u32,
            from,
            input: input.to_vec(),
            success: !out.reverted,
            gas: out.gas_used,
            ret: out.bytes.to_vec(),
            revert: revert.clone(),
            logs: indexed,
        });
        self.open_calls += 1;
        self.last_ret = out.bytes.to_vec();
        match revert {
            Some(r) => Err(r),
            None => Ok(logs),
        }
    }

    #[track_caller]
    fn ok(&mut self, from: Address, input: &[u8]) -> Vec<Log> {
        self.send(from, input).unwrap_or_else(|e| panic!("unexpected revert: {e}"))
    }

    #[track_caller]
    fn refused(&mut self, from: Address, input: &[u8], expect: &str) {
        match self.send(from, input) {
            Ok(_) => panic!("expected a revert containing {expect:?}, the call succeeded"),
            Err(e) => assert!(e.contains(expect), "{e:?} does not contain {expect:?}"),
        }
    }

    /// Ends the open block (if anything landed in it) and folds its delta into the commitment.
    fn end_block(&mut self) {
        if self.open_calls == 0 {
            return;
        }
        let delta = self.h.end_block();
        if !delta.is_empty() {
            self.commitment = perp_core::compute_block_commitment(self.commitment, &delta);
        }
        self.blocks.push(BlockRec {
            number: self.block,
            timestamp: self.h.now,
            first_call: self.calls.len() - self.open_calls,
            calls: self.open_calls,
            logs: self.log_index,
            commitment: self.commitment,
        });
        self.block += 1;
        self.open_calls = 0;
        self.log_index = 0;
    }

    /// Ends the open block and starts the next one at `t` (strictly later, as on chain).
    fn next_block(&mut self, t: u64) {
        self.end_block();
        assert!(t > self.h.now || self.blocks.is_empty(), "block timestamps must increase");
        self.h.now = t;
    }

    /// Runs `f` on a throwaway copy of the host (live store and overlay cloned, the committed store
    /// lent), so that no read warms or touches the host the calls run on.
    fn probe<R>(&mut self, f: impl FnOnce(&mut InMemoryHost) -> R) -> R {
        let mut copy = InMemoryHost::new(self.h.now).with_chain_id(self.h.chain_id);
        copy.live = self.h.live.clone();
        copy.overlay = self.h.overlay.clone();
        copy.balances = self.h.balances.clone();
        copy.committed = std::mem::take(&mut self.h.committed);
        let r = f(&mut copy);
        self.h.committed = std::mem::take(&mut copy.committed);
        r
    }

    /// A view through the shell (static), decoded. For the scenarios' own decisions.
    #[track_caller]
    fn view<C: SolCall>(&mut self, c: &C) -> C::Return {
        let input = c.abi_encode();
        let (reverted, ret) = self.probe(|h| static_call(h, &input));
        assert!(!reverted, "view {} reverted: {}", sig_name(C::SIGNATURE), revert_reason(&ret));
        C::abi_decode_returns(&ret).unwrap()
    }

    /// Ends the open block and records every getter the harness reads, at that block.
    fn checkpoint(&mut self, label: &str) {
        self.end_block();
        let actors: Vec<Address> = self.actors.iter().map(|(a, _)| *a).collect();
        let markets = self.markets.clone();
        let writes = self.h.perp_write_count();
        let reads = self.probe(|h| snapshot_reads(h, &actors, &markets));
        assert_eq!(self.h.perp_write_count(), writes, "a checkpoint read wrote");
        let after_block = self.blocks.last().map(|b| b.number).unwrap_or(0);
        self.checkpoints.push(Checkpoint {
            label: label.to_string(),
            after_block,
            after_call: self.calls.len(),
            reads,
        });
    }
}

fn static_call(h: &mut InMemoryHost, input: &[u8]) -> (bool, Vec<u8>) {
    let out = run_perp_dex_call(input, GAS, Address::ZERO, U256::ZERO, true, h)
        .unwrap_or_else(|e| panic!("a view hard-failed: {e:?}"));
    (out.reverted, out.bytes.to_vec())
}

/// The reads of one checkpoint, in a fixed order: globals, per market (with the whole book and
/// every order on it), per actor, per actor and market (with every open conditional).
fn snapshot_reads(h: &mut InMemoryHost, actors: &[Address], markets: &[u64]) -> Vec<ReadRec> {
    let mut out = Vec::new();
    fn perp<C: SolCall>(
        h: &mut InMemoryHost,
        out: &mut Vec<ReadRec>,
        c: C,
        args: &[String],
    ) -> Option<C::Return> {
        let input = c.abi_encode();
        let (reverted, ret) = static_call(h, &input);
        let decoded = if reverted { None } else { Some(C::abi_decode_returns(&ret).unwrap()) };
        out.push(ReadRec {
            to: PERP_DEX_ADDRESS,
            name: sig_name(C::SIGNATURE),
            args: jarr(args),
            input,
            ret,
            reverted,
        });
        decoded
    }
    fn erc20(h: &mut InMemoryHost, out: &mut Vec<ReadRec>, owner: Address) {
        let mut input = vec![0x70, 0xa0, 0x82, 0x31];
        input.extend_from_slice(&[0u8; 12]);
        input.extend_from_slice(owner.as_slice());
        let v = h.external_balance(USDC_ADDRESS, owner).unwrap();
        out.push(ReadRec {
            to: USDC_ADDRESS,
            name: "balanceOf",
            args: jarr(&[haddr(owner)]),
            input,
            ret: v.to_be_bytes::<32>().to_vec(),
            reverted: false,
        });
    }
    let num = |v: u64| v.to_string();

    perp(h, &mut out, getAdminCall {}, &[]);
    perp(h, &mut out, getInsuranceFundCall {}, &[]);
    perp(h, &mut out, getMaxMakerRebateCall {}, &[]);
    erc20(h, &mut out, PERP_DEX_ADDRESS);
    for &m in markets {
        let a = [num(m)];
        perp(h, &mut out, getMarketCall { marketId: m }, &a);
        perp(h, &mut out, getMarkPriceCall { marketId: m }, &a);
        perp(h, &mut out, getIndexPriceCall { marketId: m }, &a);
        perp(h, &mut out, getFundingStateCall { marketId: m }, &a);
        perp(h, &mut out, getMarketFeeTotalCall { marketId: m }, &a);
        perp(h, &mut out, getMarginTiersCall { marketId: m }, &a);
        for side in [0u8, 1] {
            let sa = [num(m), side.to_string()];
            let prices =
                perp(h, &mut out, getBookPricesCall { marketId: m, side }, &sa).unwrap_or_default();
            for price in prices {
                let la = [num(m), side.to_string(), num(price)];
                let level = getBookLevelCall { marketId: m, side, price };
                let ids = perp(h, &mut out, level, &la).unwrap_or_default();
                for id in ids {
                    let oa = [hex(id.as_slice()), num(m)];
                    perp(h, &mut out, getOrderCall { orderId: id, marketId: m }, &oa);
                }
            }
        }
    }
    for &u in actors {
        let a = [haddr(u)];
        perp(h, &mut out, getAccountCall { user: u }, &a);
        let ids: Vec<String> = markets.iter().map(|m| m.to_string()).collect();
        let am = [haddr(u), format!("[{}]", ids.join(","))];
        perp(h, &mut out, getAccountMarginCall { user: u, marketIds: markets.to_vec() }, &am);
        // Over NO market: `totalCrossWalletBalance` is then the STORED wallet `W`, with none of
        // the unsettled cross funding `getAccount` adds to it — what a mid-trace seed starts from.
        let none = [haddr(u), "[]".to_string()];
        perp(h, &mut out, getAccountMarginCall { user: u, marketIds: Vec::new() }, &none);
        perp(h, &mut out, getUserFeeRatesCall { user: u }, &a);
        perp(h, &mut out, getMasterCall { account: u }, &a);
        perp(h, &mut out, getSubAccountsCall { master: u }, &a);
        perp(h, &mut out, getApiKeysCall { user: u }, &a);
        erc20(h, &mut out, u);
    }
    for &u in actors {
        for &m in markets {
            let a = [haddr(u), num(m)];
            let pos = perp(h, &mut out, getPositionCall { user: u, marketId: m }, &a);
            if pos.is_some_and(|p| p.amount != 0) {
                // `W` plus THIS market's unsettled cross funding (0 for an ISOLATED position):
                // the seed's per-leg pending funding.
                let one = [haddr(u), format!("[{m}]")];
                let c = getAccountMarginCall { user: u, marketIds: vec![m] };
                perp(h, &mut out, c, &one);
            }
            perp(h, &mut out, getPositionRiskCall { user: u, marketId: m }, &a);
            perp(h, &mut out, getSymbolConfigCall { user: u, marketId: m }, &a);
            perp(h, &mut out, getOpenOrdersCall { user: u, marketId: m }, &a);
            let algos = perp(h, &mut out, getOpenAlgoOrdersCall { user: u, marketId: m }, &a)
                .unwrap_or_default();
            for id in algos {
                perp(h, &mut out, getAlgoOrderCall { algoId: id }, &[hex(id.as_slice())]);
            }
        }
    }
    out
}

// ── Summary + JSON emission ────────────────────────────────────────────────────────────────────

/// Event counts by name, plus the reason / margin-type splits the harness cares about.
fn event_counts(calls: &[CallRec]) -> (BTreeMap<String, u64>, BTreeMap<String, u64>) {
    let mut by_name = BTreeMap::new();
    let mut detail = BTreeMap::new();
    for c in calls {
        for (_, l) in &c.logs {
            let t = l.data.topics();
            let name = event_name(&t[0]);
            *by_name.entry(name.to_string()).or_insert(0) += 1;
            let d = &l.data.data;
            let key = match name {
                "OrderCancelled" => {
                    let e = I::OrderCancelled::decode_raw_log(t, d).unwrap();
                    Some(format!("reason={}", e.reason))
                }
                "OrderExpired" => {
                    let e = I::OrderExpired::decode_raw_log(t, d).unwrap();
                    Some(format!("reason={}", e.reason))
                }
                "AlgoOrderCancelled" => {
                    let e = I::AlgoOrderCancelled::decode_raw_log(t, d).unwrap();
                    Some(format!("reason={}", e.reason))
                }
                "AccountBalanceChanged" => {
                    let e = I::AccountBalanceChanged::decode_raw_log(t, d).unwrap();
                    Some(format!("reason={}", e.reason))
                }
                "Liquidation" => {
                    let e = I::Liquidation::decode_raw_log(t, d).unwrap();
                    Some(format!("marginType={}", e.marginType))
                }
                "PositionChanged" => {
                    let e = I::PositionChanged::decode_raw_log(t, d).unwrap();
                    Some(format!("marginType={}", e.marginType))
                }
                "MarginTypeChanged" => {
                    let e = I::MarginTypeChanged::decode_raw_log(t, d).unwrap();
                    Some(format!("marginType={}", e.marginType))
                }
                "FundingSettled" => {
                    let e = I::FundingSettled::decode_raw_log(t, d).unwrap();
                    Some(if e.amount == 0 { "amount=0".into() } else { "amount!=0".into() })
                }
                _ => None,
            };
            if let Some(k) = key {
                *detail.entry(format!("{name}.{k}")).or_insert(0) += 1;
            }
        }
    }
    (by_name, detail)
}

fn count(r: &Recorder, key: &str) -> u64 {
    let (by_name, detail) = event_counts(&r.calls);
    by_name.get(key).or_else(|| detail.get(key)).copied().unwrap_or(0)
}

fn to_json(r: &Recorder, scenario: &str, description: &str, seed: Option<u64>) -> String {
    let mut o = String::new();
    let commit = std::env::var("PERP_FIXTURE_UMI_COMMIT").unwrap_or_else(|_| "unknown".into());
    let _ = writeln!(o, "{{\"schema\":{},", js(SCHEMA));
    let _ = writeln!(o, "\"scenario\":{},", js(scenario));
    let _ = writeln!(o, "\"description\":{},", js(description));
    let seed = seed.map(|s| js(&format!("{s:#018x}"))).unwrap_or_else(|| "null".into());
    let _ = writeln!(
        o,
        "\"generator\":{{\"umiCommit\":{},\"exporter\":{},\"test\":{},\
         \"blockCommitmentVersion\":{},\"seed\":{seed},\"callGasLimit\":{GAS}}},",
        js(&commit),
        js("crates/perp-engine/src/fixture_export_tests.rs"),
        js(&format!("fixture_export_{scenario}")),
        perp_core::BLOCK_COMMITMENT_VERSION,
    );
    let _ = writeln!(
        o,
        "\"chain\":{{\"chainId\":{},\"perpDex\":{},\"usdc\":{}}},",
        r.h.chain_id,
        js(&haddr(PERP_DEX_ADDRESS)),
        js(&haddr(USDC_ADDRESS)),
    );
    let genesis: Vec<String> = r
        .genesis
        .iter()
        .map(|(a, v)| {
            format!("{{\"address\":{},\"balance\":{}}}", js(&haddr(*a)), js(&v.to_string()))
        })
        .collect();
    let _ = writeln!(o, "\"genesis\":{{\"erc20Balances\":[{}]}},", genesis.join(","));
    let actors: Vec<String> = r
        .actors
        .iter()
        .map(|(a, l)| format!("{{\"address\":{},\"label\":{}}}", js(&haddr(*a)), js(l)))
        .collect();
    let _ = writeln!(o, "\"actors\":[{}],", actors.join(","));
    let markets: Vec<String> = r.markets.iter().map(|m| m.to_string()).collect();
    let _ = writeln!(o, "\"markets\":[{}],", markets.join(","));

    let _ = writeln!(o, "\"blocks\":[");
    for (i, b) in r.blocks.iter().enumerate() {
        let sep = if i + 1 < r.blocks.len() { "," } else { "" };
        let _ = writeln!(
            o,
            "{{\"number\":{},\"timestamp\":{},\"firstCall\":{},\"callCount\":{},\"logCount\":{},\
             \"perpCommitment\":{}}}{sep}",
            b.number,
            b.timestamp,
            b.first_call,
            b.calls,
            b.logs,
            js(&hex(&b.commitment.to_be_bytes::<32>())),
        );
    }
    let _ = writeln!(o, "],");

    let _ = writeln!(o, "\"calls\":[");
    for (i, c) in r.calls.iter().enumerate() {
        let sep = if i + 1 < r.calls.len() { "," } else { "" };
        let sel: [u8; 4] = c.input[..4].try_into().unwrap();
        let logs: Vec<String> = c
            .logs
            .iter()
            .map(|(li, l)| {
                let topics: Vec<String> =
                    l.data.topics().iter().map(|t| hex(t.as_slice())).collect();
                format!(
                    "{{\"logIndex\":{li},\"address\":{},\"topics\":{},\"data\":{}}}",
                    js(&haddr(l.address)),
                    jarr(&topics),
                    js(&hex(&l.data.data)),
                )
            })
            .collect();
        let revert = c.revert.as_deref().map(js).unwrap_or_else(|| "null".into());
        let _ = writeln!(
            o,
            "{{\"i\":{i},\"block\":{},\"timestamp\":{},\"txIndex\":{},\"from\":{},\"to\":{},\
             \"selector\":{},\"fn\":{},\"calldata\":{},\"success\":{},\"gas\":{},\
             \"returnData\":{},\"revert\":{revert},\"logs\":[{}]}}{sep}",
            c.block,
            c.timestamp,
            c.tx_index,
            js(&haddr(c.from)),
            js(&haddr(PERP_DEX_ADDRESS)),
            js(&hex(&sel)),
            js(call_name(sel)),
            js(&hex(&c.input)),
            c.success,
            c.gas,
            js(&hex(&c.ret)),
            logs.join(","),
        );
    }
    let _ = writeln!(o, "],");

    let _ = writeln!(o, "\"checkpoints\":[");
    for (i, cp) in r.checkpoints.iter().enumerate() {
        let _ = writeln!(
            o,
            "{{\"label\":{},\"afterBlock\":{},\"afterCall\":{},\"reads\":[",
            js(&cp.label),
            cp.after_block,
            cp.after_call,
        );
        for (j, rd) in cp.reads.iter().enumerate() {
            let sep = if j + 1 < cp.reads.len() { "," } else { "" };
            let _ = writeln!(
                o,
                "{{\"to\":{},\"fn\":{},\"args\":{},\"calldata\":{},\"reverted\":{},\
                 \"returnData\":{}}}{sep}",
                js(&haddr(rd.to)),
                js(rd.name),
                rd.args,
                js(&hex(&rd.input)),
                rd.reverted,
                js(&hex(&rd.ret)),
            );
        }
        let sep = if i + 1 < r.checkpoints.len() { "," } else { "" };
        let _ = writeln!(o, "]}}{sep}");
    }
    let _ = writeln!(o, "],");

    // Summary.
    let (by_name, detail) = event_counts(&r.calls);
    let mut by_fn: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
    let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
    for c in &r.calls {
        let e = by_fn.entry(call_name(c.input[..4].try_into().unwrap())).or_default();
        if c.success {
            e.0 += 1;
        } else {
            e.1 += 1;
            *reasons.entry(c.revert.clone().unwrap()).or_insert(0) += 1;
        }
    }
    let map = |m: &BTreeMap<String, u64>| {
        m.iter().map(|(k, v)| format!("{}:{v}", js(k))).collect::<Vec<_>>().join(",")
    };
    let fns = by_fn
        .iter()
        .map(|(k, (ok, rv))| format!("{}:{{\"ok\":{ok},\"reverted\":{rv}}}", js(k)))
        .collect::<Vec<_>>()
        .join(",");
    let succeeded = r.calls.iter().filter(|c| c.success).count();
    let logs: usize = r.calls.iter().map(|c| c.logs.len()).sum();
    let _ = writeln!(
        o,
        "\"summary\":{{\"calls\":{},\"succeeded\":{succeeded},\"reverted\":{},\"blocks\":{},\
         \"logs\":{logs},\"checkpoints\":{},\"finalPerpCommitment\":{},\n\"callsByFn\":{{{fns}}},\n\
         \"eventCounts\":{{{}}},\n\"eventDetail\":{{{}}},\n\"revertReasons\":{{{}}}}}",
        r.calls.len(),
        r.calls.len() - succeeded,
        r.blocks.len(),
        r.checkpoints.len(),
        js(&hex(&r.commitment.to_be_bytes::<32>())),
        map(&by_name),
        map(&detail),
        map(&reasons),
    );
    o.push_str("}\n");
    o
}

/// Checks the fixture's own shape, prints its summary, and writes it when `PERP_FIXTURE_OUT` is
/// set.
fn emit(r: &Recorder, scenario: &str, description: &str, seed: Option<u64>) {
    assert_eq!(r.open_calls, 0, "the last block was not ended");
    assert!(!r.checkpoints.is_empty(), "a fixture needs at least its end checkpoint");
    assert_eq!(r.checkpoints.last().unwrap().after_call, r.calls.len(), "end checkpoint is last");
    // Block numbers and tx indexes are dense; timestamps strictly increase block to block.
    let mut expect = 0usize;
    for (k, b) in r.blocks.iter().enumerate() {
        assert_eq!(b.number, k as u64 + 1);
        assert_eq!(b.first_call, expect);
        for (t, c) in r.calls[b.first_call..b.first_call + b.calls].iter().enumerate() {
            assert_eq!((c.block, c.tx_index as usize, c.timestamp), (b.number, t, b.timestamp));
        }
        if k > 0 {
            assert!(b.timestamp > r.blocks[k - 1].timestamp, "block timestamps must increase");
        }
        expect += b.calls;
    }
    assert_eq!(expect, r.calls.len());
    let json = to_json(r, scenario, description, seed);
    assert!(json.len() < MAX_FIXTURE_BYTES, "{scenario}: {} bytes", json.len());
    let (by_name, detail) = event_counts(&r.calls);
    let ok = r.calls.iter().filter(|c| c.success).count();
    eprintln!(
        "fixture {scenario}: {} calls ({ok} ok), {} blocks, {} checkpoints, {} bytes\n  events \
         {by_name:?}\n  detail {detail:?}",
        r.calls.len(),
        r.blocks.len(),
        r.checkpoints.len(),
        json.len(),
    );
    if let Ok(dir) = std::env::var("PERP_FIXTURE_OUT") {
        std::fs::create_dir_all(&dir).unwrap();
        let path = std::path::Path::new(&dir).join(format!("{scenario}.json"));
        std::fs::write(&path, json).unwrap();
        eprintln!("  wrote {}", path.display());
    }
}

// ── (a) the isolated golden scenario ───────────────────────────────────────────────────────────

mod isolated_golden {
    use super::*;

    pub(super) const CHAIN_ID: u64 = 1;
    pub(super) const ALICE: Address = address!("1111111111111111111111111111111111111111");
    pub(super) const BOB: Address = address!("2222222222222222222222222222222222222222");
    pub(super) const CAROL: Address = address!("3333333333333333333333333333333333333333");
    pub(super) const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    const ORACLE: Address = address!("4444444444444444444444444444444444444444");
    const MANAGER: Address = address!("5555555555555555555555555555555555555555");
    const NEW_ADMIN: Address = address!("6666666666666666666666666666666666666666");
    const MARKET_ID: u64 = 1;
    const TICK: u64 = 1_000_000_000;
    const PRICE: u64 = 100 * TICK;
    const QTY: u64 = 1_000_000;
    const SIGNED_TS: u64 = 1;
    const SIGNED_RECV: u64 = 60;

    fn place(side: u8, price: u64, qty: u64, order_type: u8, tif: u8) -> Vec<u8> {
        placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: order_type,
            tif,
            clientOrderId: FixedBytes::default(),
            flags: 0,
        }
        .abi_encode()
    }

    fn signed_place(sk: &SigningKey, side: u8, price: u64, qty: u64, client: [u8; 16]) -> Vec<u8> {
        use crate::trading::{put, write_signed_header};
        let mut msg = [0u8; 105];
        let mut c = write_signed_header(&mut msg, b"perpdex_v1_order", CHAIN_ID);
        c = put(&mut msg, c, ALICE.as_slice());
        c = put(&mut msg, c, &MARKET_ID.to_be_bytes());
        c = put(&mut msg, c, &[side]);
        c = put(&mut msg, c, &price.to_be_bytes());
        c = put(&mut msg, c, &qty.to_be_bytes());
        c = put(&mut msg, c, &[0, 0]);
        c = put(&mut msg, c, &client);
        c = put(&mut msg, c, &SIGNED_TS.to_be_bytes());
        c = put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
        c = put(&mut msg, c, &[0, 0]);
        assert_eq!(c, msg.len());
        placeOrderSignedCall {
            account: ALICE,
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes(client),
            timestamp: SIGNED_TS,
            recvWindow: SIGNED_RECV,
            keyId: 0,
            signature: sk.sign(&msg).to_bytes().to_vec().into(),
            flags: 0,
        }
        .abi_encode()
    }

    fn signed_cancel(sk: &SigningKey, order_id: Id) -> Vec<u8> {
        use crate::trading::{put, write_signed_header};
        let mut msg = [0u8; 102];
        let mut c = write_signed_header(&mut msg, b"perpdex_v1_cancel", CHAIN_ID);
        c = put(&mut msg, c, ALICE.as_slice());
        c = put(&mut msg, c, &order_id);
        c = put(&mut msg, c, &MARKET_ID.to_be_bytes());
        c = put(&mut msg, c, &SIGNED_TS.to_be_bytes());
        c = put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
        c = put(&mut msg, c, &[0]);
        assert_eq!(c, msg.len());
        cancelOrderSignedCall {
            account: ALICE,
            orderId: order_id.into(),
            marketId: MARKET_ID,
            timestamp: SIGNED_TS,
            recvWindow: SIGNED_RECV,
            keyId: 0,
            signature: sk.sign(&msg).to_bytes().to_vec().into(),
        }
        .abi_encode()
    }

    fn signed_leverage(sk: &SigningKey, leverage: u64) -> Vec<u8> {
        use crate::trading::{put, write_signed_header};
        let mut msg = [0u8; 80];
        let mut c = write_signed_header(&mut msg, b"perpdex_v1_leverage", CHAIN_ID);
        c = put(&mut msg, c, ALICE.as_slice());
        c = put(&mut msg, c, &MARKET_ID.to_be_bytes());
        c = put(&mut msg, c, &leverage.to_be_bytes());
        c = put(&mut msg, c, &SIGNED_TS.to_be_bytes());
        c = put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
        c = put(&mut msg, c, &[0]);
        assert_eq!(c, msg.len());
        setLeverageSignedCall {
            account: ALICE,
            marketId: MARKET_ID,
            leverage,
            timestamp: SIGNED_TS,
            recvWindow: SIGNED_RECV,
            keyId: 0,
            signature: sk.sign(&msg).to_bytes().to_vec().into(),
        }
        .abi_encode()
    }

    fn tick(r: &mut Recorder, index: u64, ts: u64) {
        let u = updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index, timestamp: ts };
        r.ok(ORACLE, &u.abi_encode());
    }

    fn id(r: &Recorder) -> Id {
        r.last_ret[..32].try_into().unwrap()
    }

    /// `trading::tests::golden::run_golden_scenario`, call for call, minus its views (an
    /// `eth_call` is not a transaction). One block at timestamp 1, as there.
    pub(super) fn scenario() -> Recorder {
        let mut r = Recorder::new(1, CHAIN_ID);
        for (u, label) in [
            (ALICE, "alice"),
            (BOB, "bob"),
            (CAROL, "carol"),
            (ADMIN, "admin"),
            (ORACLE, "oracle"),
            (MANAGER, "market_manager"),
            (NEW_ADMIN, "new_admin"),
        ] {
            r.actor(u, label);
        }
        r.markets = vec![MARKET_ID];
        r.mint(ALICE, 2_000_000_000);
        r.mint(BOB, 2_000_000_000);
        r.mint(CAROL, 10_000_000);
        r.mint(ADMIN, 500_000_000);
        let sk = SigningKey::from_bytes(&[7u8; 32]);

        // Phase 0 — roles.
        r.ok(ADMIN, &initAdminCall { admin: ADMIN }.abi_encode());
        r.refused(ALICE, &initAdminCall { admin: ALICE }.abi_encode(), "already initialised");
        r.ok(ADMIN, &setOracleAddressCall { oracle: ORACLE }.abi_encode());
        r.ok(ADMIN, &setMarketManagerAddressCall { manager: MANAGER }.abi_encode());

        // Phase 1 — the market, then an admin updateMarket.
        let add = addMarketCall {
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
            priceBandBps: 1_000_000,
        };
        r.ok(MANAGER, &add.abi_encode());
        let update = updateMarketCall {
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
            priceBandBps: 1_000_000,
        };
        r.ok(ADMIN, &update.abi_encode());
        for user in [ALICE, BOB] {
            let rates = setUserFeeRatesCall { user, makerFeePpm: 200, takerFeePpm: 500 };
            r.ok(ADMIN, &rates.abi_encode());
        }
        let rates = setUserFeeRatesCall { user: BOB, makerFeePpm: 0, takerFeePpm: 0 };
        r.refused(BOB, &rates.abi_encode(), "not admin");

        // Phase 2 — oracle bootstrap.
        tick(&mut r, PRICE, 15);

        // Phase 3 — money in.
        for user in [ALICE, BOB] {
            r.ok(user, &depositCall { amount: U256::from(1_500_000_000u64) }.abi_encode());
            r.ok(user, &transferToPerpCall { amount: 1_000_000_000 }.abi_encode());
        }
        r.ok(ADMIN, &depositCall { amount: U256::from(200_000_000u64) }.abi_encode());
        r.ok(ADMIN, &transferToPerpCall { amount: 100_000_000 }.abi_encode());

        // Phase 3b — every trader ISOLATED on the flat market.
        for user in [ALICE, BOB, CAROL] {
            let iso = setMarginTypeCall { marketId: MARKET_ID, marginType: 1 };
            r.ok(user, &iso.abi_encode());
        }

        // Phase 4 — leverage.
        r.ok(ALICE, &setLeverageCall { marketId: MARKET_ID, leverage: 3 }.abi_encode());
        r.ok(BOB, &setLeverageCall { marketId: MARKET_ID, leverage: 2 }.abi_encode());

        // Phase 5 — the signed path (relayed by CAROL), leverage retuned both ways.
        let reg = registerApiKeyCall {
            keyId: 0,
            pubkey: FixedBytes(sk.verifying_key().to_bytes()),
            expiry: 0,
        };
        r.ok(ALICE, &reg.abi_encode());
        r.ok(CAROL, &signed_place(&sk, 0, PRICE - 5 * TICK, QTY, [0xA1; 16]));
        let signed_buy = id(&r);
        r.ok(CAROL, &signed_leverage(&sk, 2));
        r.ok(ALICE, &setLeverageCall { marketId: MARKET_ID, leverage: 3 }.abi_encode());

        // Phase 6 — direct trading.
        r.ok(BOB, &place(1, PRICE, QTY, 0, 0));
        r.ok(BOB, &place(1, PRICE, QTY, 0, 0));
        r.ok(BOB, &place(1, PRICE + TICK, QTY, 0, 0));
        r.ok(BOB, &place(1, PRICE + 2 * TICK, QTY, 0, 0));
        r.refused(ALICE, &place(0, PRICE + 2 * TICK, 5 * QTY, 0, 2), "FOK order cannot be fully");
        r.refused(ALICE, &place(0, PRICE, QTY, 0, 3), "PostOnly order would match");
        r.ok(ALICE, &place(0, 0, QTY, 1, 1));
        r.ok(ALICE, &place(0, PRICE + 2 * TICK, 3 * QTY, 0, 0));
        r.ok(ALICE, &place(0, 0, QTY, 1, 1));
        r.ok(BOB, &place(1, PRICE + 10 * TICK, QTY, 0, 3));
        let po_ask = id(&r);
        r.ok(BOB, &cancelOrderCall { orderId: po_ask.into(), marketId: MARKET_ID }.abi_encode());
        r.ok(ALICE, &place(0, PRICE - 10 * TICK, QTY, 0, 0));
        let gtc = id(&r);
        let cancel = cancelOrderCall { orderId: gtc.into(), marketId: MARKET_ID }.abi_encode();
        r.ok(ALICE, &cancel);
        r.refused(ALICE, &cancel, "order not found");
        r.ok(ALICE, &place(1, PRICE + 50 * TICK, QTY, 0, 1));
        r.ok(CAROL, &signed_cancel(&sk, signed_buy));

        // Phase 7 — isolated margin in.
        let add = addPositionMarginCall { marketId: MARKET_ID, amount: 500_000 };
        r.ok(ALICE, &add.abi_encode());

        // Phase 8 — funding: a mid-epoch sample, the epoch boundary, a stale update.
        tick(&mut r, PRICE, 30);
        tick(&mut r, PRICE, 3_615);
        tick(&mut r, PRICE + TICK, 3_616);
        let remove = removePositionMarginCall { marketId: MARKET_ID, amount: 250_000 };
        r.ok(ALICE, &remove.abi_encode());

        // Phase 9 — insurance fund, crash, liquidation.
        r.ok(ADMIN, &depositInsuranceFundCall { amount: 50_000_000 }.abi_encode());
        r.ok(ADMIN, &withdrawInsuranceFundCall { amount: 1_000_000 }.abi_encode());
        r.refused(BOB, &withdrawInsuranceFundCall { amount: 1 }.abi_encode(), "not admin");
        r.ok(ALICE, &place(0, PRICE - 30 * TICK, QTY, 0, 0));
        r.ok(ALICE, &place(1, PRICE, QTY, 0, 0));
        r.ok(BOB, &place(0, PRICE - 20 * TICK, 3 * QTY, 0, 0));
        tick(&mut r, PRICE - 30 * TICK, 3_630);
        let liq = liquidateCall { user: ALICE, marketId: MARKET_ID };
        r.refused(CAROL, &liq.abi_encode(), "liquidate: no open position");

        // Phase 9b — CAROL funds up and closes BOB's residual short through the head bid.
        r.ok(CAROL, &depositCall { amount: U256::from(10_000_000u64) }.abi_encode());
        r.ok(CAROL, &transferToPerpCall { amount: 5_000_000 }.abi_encode());
        r.ok(BOB, &place(0, PRICE - 20 * TICK, QTY, 0, 0));
        r.ok(BOB, &place(0, PRICE - 20 * TICK, QTY, 0, 0));
        r.ok(CAROL, &place(1, PRICE - 20 * TICK, QTY, 0, 0));

        // Phase 9c — a resting sell against a non-flat position, priced at the assuming price.
        tick(&mut r, PRICE - 10 * TICK, 3_645);
        r.ok(CAROL, &place(1, PRICE - 19 * TICK, QTY, 0, 0));

        // Phase 9d — an untriggered reduce-only stop.
        let stop = placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: 0,
            orderType: 1,
            triggerPrice: PRICE + 20 * TICK,
            price: 0,
            quantity: QTY,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: 1,
        };
        r.ok(CAROL, &stop.abi_encode());

        // Phase 10 — solvent exit, key revoked, admin handed over.
        r.ok(BOB, &transferFromPerpCall { amount: 500_000_000 }.abi_encode());
        r.ok(BOB, &withdrawCall { amount: U256::from(500_000_000u64) }.abi_encode());
        r.ok(ALICE, &revokeApiKeyCall { keyId: 0 }.abi_encode());
        let zero = transferAdminCall { newAdmin: Address::ZERO };
        r.refused(ADMIN, &zero.abi_encode(), "cannot be zero address");
        let bob = transferAdminCall { newAdmin: BOB };
        r.refused(BOB, &bob.abi_encode(), "caller is not admin");
        r.ok(ADMIN, &transferAdminCall { newAdmin: NEW_ADMIN }.abi_encode());
        r
    }
}

#[test]
#[ignore = "fixture exporter — PERP_FIXTURE_OUT=<dir> cargo test -p perp-engine --lib \
            fixture_export -- --ignored"]
fn fixture_export_isolated_golden() {
    let mut r = isolated_golden::scenario();
    r.checkpoint("end: the golden scenario's end state");
    assert_eq!(r.blocks.len(), 1, "the golden scenario is one block");
    assert_eq!(
        B256::from(r.commitment.to_be_bytes::<32>()),
        crate::trading::tests::golden::GOLDEN_COMMITMENT,
        "the replay is not the golden scenario write for write"
    );
    emit(
        &r,
        "isolated_golden",
        "trading::tests::golden (commitment_golden_scenario), call for call, one block at \
         timestamp 1; its block commitment equals the pinned GOLDEN_COMMITMENT. ISOLATED margin: \
         roles, one market (8/9 decimals), fee rates, signed place / cancel / leverage, FOK and \
         PostOnly refusals, market / limit / IOC / PostOnly orders, cancels, isolated margin in \
         and out, a funding epoch, the insurance fund, an auto-liquidation with cancel-all and a \
         residual, an untriggered reduce-only stop, withdraw, transferAdmin.",
        None,
    );
}

// ── (b) the cross golden scenario ──────────────────────────────────────────────────────────────

mod cross_golden {
    use super::*;

    const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    const ORACLE: Address = address!("0000000000000000000000000000000000000c1e");
    const ALICE: Address = address!("1111111111111111111111111111111111111111");
    const BOB: Address = address!("2222222222222222222222222222222222222222");
    const DAVE: Address = address!("4444444444444444444444444444444444444444");
    const ERIN: Address = address!("5555555555555555555555555555555555555555");
    const MAKER: Address = address!("7777777777777777777777777777777777777777");
    const M1: u64 = 1;
    const M2: u64 = 2;
    const M3: u64 = 3;
    const MARKETS: [u64; 3] = [M1, M2, M3];
    const USD: u64 = 1_000_000;
    const BUY: u8 = 0;
    const SELL: u8 = 1;
    const GTC: u8 = 0;
    const IOC: u8 = 1;
    const ISOLATED: u8 = 1;

    const fn px(dollars: u64) -> u64 {
        dollars * 100
    }

    fn order(market: u64, side: u8, price: u64, qty: u64, tif: u8, flags: u8) -> Vec<u8> {
        placeOrderCall {
            marketId: market,
            side,
            price,
            quantity: qty,
            orderType: 0,
            tif,
            clientOrderId: FixedBytes::default(),
            flags,
        }
        .abi_encode()
    }

    /// The block clock moves to `t`. `split`: a new block, the one before it checkpointed as
    /// `label`; otherwise the clock moves inside the one block, as in `cross_golden_tests`.
    fn at(r: &mut Recorder, split: bool, t: u64, label: &str) {
        if r.h.now == t {
            return;
        }
        if split {
            r.checkpoint(label);
            r.next_block(t);
        } else {
            r.h.now = t;
        }
    }

    fn tick(r: &mut Recorder, split: bool, m: u64, dollars: u64, t: u64, label: &str) {
        at(r, split, t, label);
        let u = updateIndexPriceCall { marketId: m, indexPrice: px(dollars), timestamp: t };
        r.ok(ORACLE, &u.abi_encode());
    }

    /// `cross_golden_tests::run_cross_golden_scenario`, call for call, minus its views.
    pub(super) fn scenario(split: bool) -> Recorder {
        let mut rec = Recorder::new(900, 1);
        let r = &mut rec;
        for (u, label) in [
            (ADMIN, "admin"),
            (ORACLE, "oracle"),
            (ALICE, "alice"),
            (BOB, "bob"),
            (DAVE, "dave"),
            (ERIN, "erin"),
            (MAKER, "maker"),
        ] {
            r.actor(u, label);
        }
        r.markets = MARKETS.to_vec();
        for (user, dollars) in [
            (ALICE, 1_000),
            (BOB, 5_000),
            (DAVE, 700),
            (ERIN, 1_000),
            (MAKER, 100_000),
            (ADMIN, 2_000),
        ] {
            r.mint(user, dollars * USD);
        }

        // Phase 0 (t = 900) — roles, the three markets, fee rates.
        r.ok(ADMIN, &initAdminCall { admin: ADMIN }.abi_encode());
        r.ok(ADMIN, &setOracleAddressCall { oracle: ORACLE }.abi_encode());
        for m in MARKETS {
            let add = addMarketCall {
                marketId: m,
                baseDecimals: 0,
                priceDecimals: 2,
                tickSize: 1,
                stepSize: 1,
                minQuantity: 1,
                maxQuantity: 1_000_000,
                maxPrice: px(1_000_000),
                priceUpdateInterval: 15,
                fundingInterval: if m == M1 { 3_600 } else { 0 },
                interestRate: 100,
                liquidationFeeRateBps: 125,
                initialMarkPrice: px(100),
                priceBandBps: 1_000_000,
            };
            r.ok(ADMIN, &add.abi_encode());
        }
        for user in [ALICE, BOB] {
            let rates = setUserFeeRatesCall { user, makerFeePpm: 200, takerFeePpm: 500 };
            r.ok(ADMIN, &rates.abi_encode());
        }

        // Phase 1 — money in; the admin seeds the insurance fund.
        for (user, dollars) in [
            (ALICE, 600),
            (BOB, 5_000),
            (DAVE, 700),
            (ERIN, 1_000),
            (MAKER, 100_000),
            (ADMIN, 2_000),
        ] {
            let spot = if user == ALICE { 1_000 } else { dollars };
            r.ok(user, &depositCall { amount: U256::from(spot * USD) }.abi_encode());
            r.ok(user, &transferToPerpCall { amount: dollars * USD }.abi_encode());
        }
        r.ok(ADMIN, &depositInsuranceFundCall { amount: 1_000 * USD }.abi_encode());

        // Phase 2 — the first ticks on M1 and M3 (M2's oracle never publishes).
        let l0 = "t=900: roles, markets, money in, first ticks, margin types, positions, resting \
                  bids";
        tick(r, split, M1, 100, 900, l0);
        tick(r, split, M3, 100, 900, l0);

        // Phase 3 — margin types (the default is CROSSED; only these go ISOLATED) and leverage.
        for m in MARKETS {
            r.ok(BOB, &setMarginTypeCall { marketId: m, marginType: ISOLATED }.abi_encode());
        }
        r.ok(ALICE, &setMarginTypeCall { marketId: M2, marginType: ISOLATED }.abi_encode());
        for (user, m) in [(ALICE, M1), (DAVE, M2), (DAVE, M3), (ERIN, M3)] {
            r.ok(user, &setLeverageCall { marketId: m, leverage: 3 }.abi_encode());
        }

        // Phase 4 — positions; resting bids.
        r.ok(BOB, &order(M1, SELL, px(100), 10, GTC, 0));
        r.ok(ALICE, &order(M1, BUY, px(100), 10, IOC, 0));
        r.ok(MAKER, &order(M2, SELL, px(100), 10, GTC, 0));
        r.ok(DAVE, &order(M2, BUY, px(100), 10, IOC, 0));
        r.ok(ERIN, &order(M2, BUY, px(100), 2, GTC, 0));
        r.ok(ALICE, &order(M2, SELL, px(100), 2, IOC, 0));
        r.ok(BOB, &order(M3, BUY, px(100), 10, GTC, 0));
        r.ok(DAVE, &order(M3, SELL, px(100), 10, IOC, 0));
        r.ok(MAKER, &order(M3, SELL, px(190), 6, GTC, 0));
        r.ok(ERIN, &order(M3, BUY, px(190), 6, IOC, 0));
        r.ok(ALICE, &order(M1, BUY, px(50), 1, GTC, 0));
        r.ok(ALICE, &order(M2, BUY, px(60), 1, GTC, 0));
        r.ok(MAKER, &order(M1, BUY, px(70), 4, GTC, 0));

        // Phase 5 — M1's funding epoch.
        tick(r, split, M1, 100, 1_800, l0);
        tick(r, split, M3, 100, 1_800, l0);
        let l1 = "t=1800: mid-epoch ticks on M1 and M3";
        tick(r, split, M1, 100, 4_500, l1);
        tick(r, split, M3, 100, 4_500, l1);

        // Phase 6 (t = 4515) — M1 crashes: ALICE's PARTIAL cross liquidation.
        let l2 = "t=4500: M1's funding epoch rolls over";
        tick(r, split, M1, 70, 4_515, l2);
        assert_eq!(count(r, "CrossLiquidationEnded"), 1, "ALICE's episode ended");

        // Phase 7 (t = 4530) — M3 spikes: DAVE bankrupt, ADL, the stale M2 leg skipped.
        let l3 = "t=4515: M1 crashes; ALICE's partial cross liquidation (cancel-all, IOC at the \
                  bankruptcy price, fee, flag ended)";
        tick(r, split, M3, 180, 4_530, l3);
        assert_eq!(count(r, "Adl"), 1, "DAVE's M3 short ADL-ed");

        // Phase 8 (t = 4545) — flagged, DAVE may only reduce; his reduce-only close ends the
        // episode and the insurance fund clears his negative wallet.
        let l4 = "t=4530: M3 spikes; DAVE bankrupt, ADL against BOB at the mark, stale M2 leg \
                  skipped, flag stays";
        at(r, split, 4_545, l4);
        let plain = order(M2, SELL, px(100), 1, IOC, 0);
        r.refused(DAVE, &plain, "-2023 User in liquidation mode now.");
        r.ok(MAKER, &order(M2, BUY, px(100), 10, GTC, 0));
        r.ok(DAVE, &order(M2, SELL, px(100), 10, IOC, 1));
        assert_eq!(count(r, "CrossLiquidationEnded"), 2, "DAVE's episode ended too");
        rec
    }
}

#[test]
#[ignore = "fixture exporter — PERP_FIXTURE_OUT=<dir> cargo test -p perp-engine --lib \
            fixture_export -- --ignored"]
fn fixture_export_cross_golden() {
    // The scenario as pinned: one block, the clock moving inside it.
    let mut pin = cross_golden::scenario(false);
    pin.end_block();
    assert_eq!(pin.blocks.len(), 1);
    assert_eq!(
        B256::from(pin.commitment.to_be_bytes::<32>()),
        crate::cross_golden_tests::CROSS_GOLDEN_COMMITMENT,
        "the one-block replay is not the cross golden scenario write for write"
    );
    pin.checkpoint("end");
    // The fixture: a block per clock value, every block checkpointed.
    let mut r = cross_golden::scenario(true);
    r.checkpoint(
        "t=4545 (end): DAVE refused (-2023), closes his last leg reduce-only; the episode ends and \
         the insurance fund clears his negative wallet",
    );
    assert_eq!(r.blocks.len(), 6);
    let (a, b) = (&pin.checkpoints[0].reads, &r.checkpoints.last().unwrap().reads);
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b) {
        assert_eq!((x.name, &x.input), (y.name, &y.input));
        assert_eq!(x.ret, y.ret, "{}{}: block boundaries moved the end state", x.name, x.args);
    }
    emit(
        &r,
        "cross_golden",
        "cross_golden_tests (cross_golden_scenario), call for call, a new block whenever the clock \
         moves (that scenario moves it inside one block; the one-block run reproduces the pinned \
         CROSS_GOLDEN_COMMITMENT and the split run ends with every getter byte-identical). Three \
         markets, MIXED accounts (ALICE CROSSED M1 / ISOLATED M2, BOB ISOLATED, DAVE / ERIN / \
         MAKER CROSSED), M1 funding settled into the cross wallet, ALICE's partial cross \
         liquidation with cancel-all (her ISOLATED market's order too), DAVE bankrupt with ADL \
         against an isolated winner, a stale market skipped (Q10), -2023, the flag ended by a \
         reduce-only close, and the negative-wallet clearance (AccountBalanceChanged reason 7).",
        None,
    );
}

// ── (c) / (d) the seeded random trace ──────────────────────────────────────────────────────────

mod random_trace {
    use super::*;

    const ADMIN: Address = address!("a0000000000000000000000000000000000000ad");
    const ORACLE: Address = address!("a0000000000000000000000000000000000000c1");
    const KEEPER: Address = address!("a0000000000000000000000000000000000000ee");
    /// Deep liquidity, ISOLATED at leverage 1 everywhere (both modes).
    const LP1: Address = address!("b000000000000000000000000000000000000001");
    /// Deep liquidity at leverage 1: CROSSED in the mixed trace, ISOLATED in the isolated one.
    const LP2: Address = address!("b000000000000000000000000000000000000002");
    const LPS: [Address; 2] = [LP1, LP2];
    /// The scenes' own accounts (never picked by a random step, so each scene starts from a state
    /// it knows): X1 the liquidation with cancel-all (CROSSED in the mixed trace), X2 the bankrupt
    /// short ADL-ed (CROSSED in the mixed trace), X3 the fat-finger close (CROSSED in the mixed
    /// trace: the negative-wallet clearance), X4 an ISOLATED long liquidated by X1's crash.
    const X1: Address = address!("d000000000000000000000000000000000000001");
    const X2: Address = address!("d000000000000000000000000000000000000002");
    pub(super) const X3: Address = address!("d000000000000000000000000000000000000003");
    const X4: Address = address!("d000000000000000000000000000000000000004");
    const SCENE_ACTORS: [(Address, &str, u64); 4] = [
        (X1, "x1_liquidated", 4_000),
        (X2, "x2_bankrupt", 20_000),
        (X3, "x3_fat_finger", 10_000),
        (X4, "x4_isolated_liquidated", 4_000),
    ];
    /// 2025-10-26 00:00 UTC: a multiple of every interval below (15 s, 3_600 s, 28_800 s).
    const T0: u64 = 1_761_436_800;
    const USD: u64 = 1_000_000;
    const BUY: u8 = 0;
    const SELL: u8 = 1;
    const LIMIT: u8 = 0;
    const MKT: u8 = 1;
    const GTC: u8 = 0;
    const IOC: u8 = 1;
    const FOK: u8 = 2;
    const POST_ONLY: u8 = 3;
    const RO: u8 = 0b01;
    const CLOSE_POSITION: u8 = 0b10;
    const CROSSED: u8 = 0;
    const ISOLATED: u8 = 1;
    /// `internalTransfer` wallet code of the perp wallet.
    const PERP_WALLET: u8 = 1;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(super) enum Mode {
        /// CROSSED by default, a third of (trader, market) pairs ISOLATED, switches allowed.
        Mixed,
        /// Every trading actor ISOLATED on every market before it trades; no switch back.
        Isolated,
    }

    /// A market: shaped like the devnet's BTC / ETH / SOL (`scripts/setup.js`), with funding fast
    /// enough to roll over inside the trace, the clearance fee on two of them, and a live price
    /// band on one. All three on the harness's 3 / 2 decimals (the devnet SOL is 2 / 2): its shadow
    /// ledger values every notional with ONE decimals pair per run.
    #[derive(Clone, Copy)]
    struct Spec {
        id: u64,
        bd: u32,
        pd: u32,
        tick: u64,
        max_qty: u64,
        max_price: u64,
        mark0: u64,
        funding_interval: u64,
        interest: i64,
        fee_bps: u32,
        band_bps: u32,
        /// `(lower bound in dollars, max leverage)`, MM derived; empty = `addMarket`'s default
        /// (one bracket at 3x).
        tiers: &'static [(u64, u32)],
    }

    const SPECS: [Spec; 3] = [
        Spec {
            id: 2,
            bd: 3,
            pd: 2,
            tick: 10,
            max_qty: 1_000_000,
            max_price: 100_000_000,
            mark0: 10_000_000,
            funding_interval: 3_600,
            interest: 100,
            fee_bps: 125,
            band_bps: 1_000_000,
            tiers: &[(0, 20), (50_000, 10), (250_000, 5)],
        },
        Spec {
            id: 4,
            bd: 3,
            pd: 2,
            tick: 1,
            max_qty: 10_000_000,
            max_price: 10_000_000,
            mark0: 300_000,
            funding_interval: 3_600,
            interest: -50,
            fee_bps: 50,
            band_bps: 1_000,
            tiers: &[(0, 10)],
        },
        Spec {
            id: 6,
            bd: 3,
            pd: 2,
            tick: 1,
            max_qty: 100_000_000,
            max_price: 1_000_000,
            mark0: 15_000,
            funding_interval: 0,
            interest: 0,
            fee_bps: 0,
            band_bps: 1_000_000,
            tiers: &[],
        },
    ];

    fn max_lev(s: &Spec) -> u64 {
        s.tiers.first().map(|t| t.1 as u64).unwrap_or(3)
    }

    /// The largest notional (dollars) a scene opens at `max_lev`: 90 % of the first tier.
    fn first_tier_cap(s: &Spec) -> u64 {
        s.tiers.get(1).map(|t| t.0 * 9 / 10).unwrap_or(u64::MAX)
    }

    fn trader(i: u8) -> Address {
        let mut b = [0u8; 20];
        b[0] = 0xc0;
        b[19] = i;
        Address::from(b)
    }

    fn snap(p: u64, s: &Spec) -> u64 {
        (p.min(s.max_price) / s.tick * s.tick).max(s.tick)
    }

    /// `mk` moved by `bps` (up or down), on market `k`'s grid.
    fn off(k: usize, mk: u64, bps: u64, up: bool) -> u64 {
        let d = (mk as u128 * bps as u128 / 10_000) as u64;
        snap(if up { mk + d } else { mk.saturating_sub(d) }, &SPECS[k])
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }

        fn pick<T: Copy>(&mut self, v: &[T]) -> T {
            v[self.below(v.len() as u64) as usize]
        }

        fn chance(&mut self, num: u64, den: u64) -> bool {
            self.below(den) < num
        }
    }

    fn order(m: u64, side: u8, price: u64, qty: u64, kind: u8, tif: u8, flags: u8) -> Vec<u8> {
        placeOrderCall {
            marketId: m,
            side,
            price,
            quantity: qty,
            orderType: kind,
            tif,
            clientOrderId: FixedBytes::default(),
            flags,
        }
        .abi_encode()
    }

    fn item(m: u64, side: u8, price: u64, qty: u64, kind: u8, tif: u8, flags: u8) -> PlaceItem {
        PlaceItem {
            marketId: m,
            side,
            price,
            quantity: qty,
            orderType: kind,
            tif,
            clientOrderId: FixedBytes::default(),
            flags,
        }
    }

    /// A conditional: `kind` 1 STOP_MARKET / 3 TAKE_PROFIT_MARKET (`flags` RO or CLOSE_POSITION,
    /// `qty` 0 with CLOSE_POSITION), or 4 TRAILING_STOP_MARKET (`price` = callback bps).
    fn algo(m: u64, side: u8, kind: u8, trigger: u64, price: u64, qty: u64, flags: u8) -> Vec<u8> {
        placeAlgoOrderCall {
            algoType: 0,
            marketId: m,
            side,
            orderType: kind,
            triggerPrice: trigger,
            price,
            quantity: qty,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags,
        }
        .abi_encode()
    }

    pub(super) struct Gen {
        pub(super) r: Recorder,
        rng: Rng,
        mode: Mode,
        /// Every account that trades by its own decision: the six traders and their subs.
        traders: Vec<Address>,
        /// `(master, sub)`.
        subs: Vec<(Address, Address)>,
        index: [u64; 3],
        last_tick: [u64; 3],
        /// The markets in play, as indexes into [`SPECS`].
        ks: Vec<usize>,
    }

    impl Gen {
        /// Setup on the markets `ks` (indexes into [`SPECS`]), checkpointed as "setup".
        pub(super) fn new(seed: u64, mode: Mode, ks: &[usize]) -> Gen {
            let mut r = Recorder::new(T0, 1);
            let mut rng = Rng(seed);
            r.markets = ks.iter().map(|&k| SPECS[k].id).collect();
            for (a, l) in [(ADMIN, "admin"), (ORACLE, "oracle"), (KEEPER, "keeper")] {
                r.actor(a, l);
            }
            r.actor(LP1, "lp1");
            r.actor(LP2, "lp2");
            let traders: Vec<Address> = (1..=6).map(trader).collect();
            for (i, t) in traders.iter().enumerate() {
                r.actor(*t, &format!("t{}", i + 1));
            }
            for (x, label, _) in SCENE_ACTORS {
                r.actor(x, label);
            }
            r.mint(ADMIN, 5_000_000 * USD);
            for lp in LPS {
                r.mint(lp, 2_000_000_000 * USD);
            }
            for t in &traders {
                r.mint(*t, (20_000 + rng.below(180_000)) * USD);
            }
            for (x, _, dollars) in SCENE_ACTORS {
                r.mint(x, dollars * USD);
            }
            let mut g = Gen {
                r,
                rng,
                mode,
                traders: traders.clone(),
                subs: Vec::new(),
                index: [0; 3],
                last_tick: [0; 3],
                ks: ks.to_vec(),
            };

            // Block 1 — roles, markets and their tiers, the fee schedule, the insurance fund, the
            // first ticks.
            g.r.ok(ADMIN, &initAdminCall { admin: ADMIN }.abi_encode());
            g.r.ok(ADMIN, &setOracleAddressCall { oracle: ORACLE }.abi_encode());
            for s in ks.iter().map(|&k| SPECS[k]) {
                let add = addMarketCall {
                    marketId: s.id,
                    baseDecimals: s.bd,
                    priceDecimals: s.pd,
                    tickSize: s.tick,
                    stepSize: 1,
                    minQuantity: 1,
                    maxQuantity: s.max_qty,
                    maxPrice: s.max_price,
                    priceUpdateInterval: 15,
                    fundingInterval: s.funding_interval,
                    interestRate: s.interest,
                    liquidationFeeRateBps: s.fee_bps,
                    initialMarkPrice: s.mark0,
                    priceBandBps: s.band_bps,
                };
                g.r.ok(ADMIN, &add.abi_encode());
                if !s.tiers.is_empty() {
                    let tiers = setMarginTiersCall {
                        marketId: s.id,
                        lowerBounds: s.tiers.iter().map(|t| t.0 * USD).collect(),
                        maxLeverages: s.tiers.iter().map(|t| t.1).collect(),
                        maintMarginRatesPpm: s.tiers.iter().map(|_| 0).collect(),
                        notionalCap: 0,
                    };
                    g.r.ok(ADMIN, &tiers.abi_encode());
                }
            }
            g.r.ok(ADMIN, &setMaxMakerRebateCall { maxMakerRebatePpm: 100 }.abi_encode());
            for (i, t) in traders.iter().enumerate() {
                let maker = if i % 2 == 0 { -50 } else { 200 };
                let rates = setUserFeeRatesCall { user: *t, makerFeePpm: maker, takerFeePpm: 500 };
                g.r.ok(ADMIN, &rates.abi_encode());
            }
            g.r.ok(ADMIN, &depositCall { amount: U256::from(3_000_000 * USD) }.abi_encode());
            g.r.ok(ADMIN, &transferToPerpCall { amount: 2_000_000 * USD }.abi_encode());
            g.r.ok(ADMIN, &depositInsuranceFundCall { amount: 1_000_000 * USD }.abi_encode());
            for &k in ks {
                assert!(g.tick(k, SPECS[k].mark0));
            }

            // Block 2 — money in.
            g.r.next_block(T0 + 3);
            for lp in LPS {
                g.r.ok(lp, &depositCall { amount: U256::from(1_000_000_000 * USD) }.abi_encode());
                g.r.ok(lp, &transferToPerpCall { amount: 1_000_000_000 * USD }.abi_encode());
            }
            for t in traders.clone() {
                let held = g.erc20(t);
                let spot = held / 10 * (7 + g.rng.below(3));
                g.r.ok(t, &depositCall { amount: U256::from(spot) }.abi_encode());
                let perp = spot / 10 * (6 + g.rng.below(4));
                g.r.ok(t, &transferToPerpCall { amount: perp }.abi_encode());
            }
            for (x, _, dollars) in SCENE_ACTORS {
                g.r.ok(x, &depositCall { amount: U256::from(dollars * USD) }.abi_encode());
                g.r.ok(x, &transferToPerpCall { amount: dollars * USD }.abi_encode());
            }

            // Block 3 — margin types, leverage, sub-accounts.
            g.r.next_block(T0 + 6);
            for s in ks.iter().map(|&k| SPECS[k]) {
                let iso = setMarginTypeCall { marketId: s.id, marginType: ISOLATED };
                g.r.ok(LP1, &iso.abi_encode());
                if mode == Mode::Isolated {
                    g.r.ok(LP2, &iso.abi_encode());
                }
                for lp in LPS {
                    g.r.ok(lp, &setLeverageCall { marketId: s.id, leverage: 1 }.abi_encode());
                }
            }
            for t in traders.clone() {
                g.configure(t);
            }
            // The scene accounts: X1–X3 keep the default (CROSSED) in the mixed trace, X4 is
            // ISOLATED in both; every one at its markets' top leverage.
            for (x, _, _) in SCENE_ACTORS {
                for s in ks.iter().map(|&k| SPECS[k]) {
                    if mode == Mode::Isolated || x == X4 {
                        let iso = setMarginTypeCall { marketId: s.id, marginType: ISOLATED };
                        g.r.ok(x, &iso.abi_encode());
                    }
                    let lev = setLeverageCall { marketId: s.id, leverage: max_lev(&s) };
                    g.r.ok(x, &lev.abi_encode());
                }
            }
            for master in [traders[0], traders[3]] {
                g.r.ok(master, &createSubAccountCall { name: "sub".into() }.abi_encode());
                let sub = createSubAccountCall::abi_decode_returns(&g.r.last_ret).unwrap();
                let label = format!("{}.sub", g.label(master));
                g.r.actor(sub, &label);
                g.subs.push((master, sub));
                g.traders.push(sub);
                let x = g.r.view(&getAccountCall { user: master }).maxWithdrawAmount;
                let amount = ((10_000 + g.rng.below(20_000)) * USD).min(x / 2);
                let to_sub = internalTransferCall {
                    to: sub,
                    fromWallet: PERP_WALLET,
                    toWallet: PERP_WALLET,
                    amount,
                };
                g.r.ok(master, &to_sub.abi_encode());
                g.configure(sub);
            }
            g.r.checkpoint(
                "setup: roles, markets, fees, insurance fund, money in, margin types, leverage, \
                 sub-accounts",
            );
            g
        }

        fn label(&self, a: Address) -> String {
            self.r.actors.iter().find(|(x, _)| *x == a).unwrap().1.clone()
        }

        /// Margin type (by mode) and a random leverage on every market, through `asSubAccount` for
        /// a sub.
        fn configure(&mut self, u: Address) {
            for s in self.ks.clone().into_iter().map(|k| SPECS[k]) {
                let iso = match self.mode {
                    Mode::Isolated => true,
                    Mode::Mixed => self.rng.chance(1, 3),
                };
                if iso {
                    let input = setMarginTypeCall { marketId: s.id, marginType: ISOLATED };
                    self.call(u, input.abi_encode()).unwrap();
                }
                let cap = max_lev(&s);
                let lev = if self.rng.chance(1, 2) { cap } else { 1 + self.rng.below(cap) };
                let set = setLeverageCall { marketId: s.id, leverage: lev };
                self.call(u, set.abi_encode()).unwrap();
            }
        }

        fn master_of(&self, u: Address) -> Option<Address> {
            self.subs.iter().find(|(_, s)| *s == u).map(|(m, _)| *m)
        }

        /// `u`'s call, through `asSubAccount` when `u` is a sub.
        fn call(&mut self, u: Address, input: Vec<u8>) -> Result<Vec<Log>, String> {
            match self.master_of(u) {
                Some(master) => {
                    let wrapped = asSubAccountCall { subAccount: u, data: Bytes::from(input) };
                    self.r.send(master, &wrapped.abi_encode())
                }
                None => self.r.send(u, &input),
            }
        }

        /// A placement; the order id on success.
        #[allow(clippy::too_many_arguments)]
        fn place(
            &mut self,
            u: Address,
            m: u64,
            side: u8,
            price: u64,
            qty: u64,
            kind: u8,
            tif: u8,
            flags: u8,
        ) -> Option<Id> {
            let input = order(m, side, price, qty, kind, tif, flags);
            self.call(u, input).ok().map(|_| self.r.last_ret[..32].try_into().unwrap())
        }

        fn erc20(&mut self, u: Address) -> u64 {
            self.r.h.external_balance(USDC_ADDRESS, u).unwrap().saturating_to()
        }

        /// `u`'s spot (deposited, not in the perp wallet) balance.
        fn spot(&mut self, u: Address) -> u64 {
            self.r.view(&getAccountCall { user: u }).usdcBalance.saturating_to()
        }

        fn mark(&mut self, m: u64) -> u64 {
            self.r.view(&getMarkPriceCall { marketId: m })
        }

        fn amount(&mut self, u: Address, m: u64) -> i64 {
            self.r.view(&getPositionCall { user: u, marketId: m }).amount
        }

        fn margin_type(&mut self, u: Address, m: u64) -> u8 {
            self.r.view(&getSymbolConfigCall { user: u, marketId: m }).marginType
        }

        fn open_orders(&mut self, u: Address, m: u64) -> Vec<Id> {
            let o = self.r.view(&getOpenOrdersCall { user: u, marketId: m });
            o.orderIds.iter().map(|id| id.0).collect()
        }

        fn is_open(&mut self, u: Address, m: u64, id: Id) -> bool {
            self.open_orders(u, m).contains(&id)
        }

        /// Size worth about `usd` dollars at the mark.
        fn qty_for(&mut self, k: usize, usd: u64) -> u64 {
            let s = SPECS[k];
            let mark = self.mark(s.id).max(1) as u128;
            let q = usd as u128 * 10u128.pow(s.bd + s.pd) / mark;
            (q as u64).clamp(1, s.max_qty)
        }

        fn rand_qty(&mut self, k: usize, usd: u64) -> u64 {
            let cap = self.qty_for(k, usd);
            1 + self.rng.below(cap)
        }

        // ── the oracle ──

        /// The oracle publishes `index` for market `k` at the block's aligned timestamp. `false`
        /// when that interval has already been published (the chain would ignore it).
        fn tick(&mut self, k: usize, index: u64) -> bool {
            let s = SPECS[k];
            let ts = self.r.h.now - self.r.h.now % 15;
            if ts <= self.last_tick[k] {
                return false;
            }
            let index = index.clamp(1, s.max_price);
            let u = updateIndexPriceCall { marketId: s.id, indexPrice: index, timestamp: ts };
            self.r.ok(ORACLE, &u.abi_encode());
            self.index[k] = index;
            self.last_tick[k] = ts;
            true
        }

        /// One block's oracle round: mostly small moves, sometimes a gap, now and then a
        /// market-wide crash or spike.
        fn oracle_round(&mut self) {
            let wide = self.rng.chance(1, 45);
            let down = self.rng.chance(3, 4);
            for k in self.ks.clone() {
                let cur = self.index[k];
                let (bps, up) = if wide {
                    (800 + self.rng.below(1_900), !down)
                } else if self.rng.chance(1, 2) {
                    let roll = self.rng.below(100);
                    let bps = match roll {
                        0..=59 => 5 + self.rng.below(100),
                        60..=89 => 100 + self.rng.below(300),
                        _ => 400 + self.rng.below(800),
                    };
                    (bps, self.rng.chance(1, 2))
                } else {
                    continue;
                };
                let d = (cur as u128 * bps as u128 / 10_000) as u64;
                let next = if up { cur + d } else { cur.saturating_sub(d) };
                self.tick(k, next);
            }
        }

        // ── the steps ──

        fn step(&mut self) {
            let k = self.rng.pick(&self.ks.clone());
            let m = SPECS[k].id;
            let u = self.rng.pick(&self.traders.clone());
            let mk = self.mark(m);
            match self.rng.below(100) {
                0..=29 => self.taker(u, k, mk),
                30..=41 => self.limit(u, k, mk),
                42..=45 => {
                    // A provider takes whatever rests (the traders' maker fills).
                    let side = self.rng.pick(&[BUY, SELL]);
                    let q = self.rand_qty(k, 60_000);
                    let lp = self.rng.pick(&LPS);
                    let _ = self.place(lp, m, side, 0, q, MKT, GTC, 0);
                }
                46..=49 => self.close(u, k, mk),
                50..=55 => self.cancels(u, m),
                56..=57 => {
                    let lev = 1 + self.rng.below(max_lev(&SPECS[k]));
                    let set = setLeverageCall { marketId: m, leverage: lev };
                    let _ = self.call(u, set.abi_encode());
                }
                58..=63 => self.money(u),
                64..=66 => self.switch_type(u, m),
                67..=69 => self.isolated_margin(u, m),
                70..=75 => self.conditional(u, k, mk),
                76..=78 => self.batch(u),
                79..=80 => {
                    let v = self.rng.pick(&self.traders.clone());
                    let liq = liquidateCall { user: v, marketId: m };
                    let _ = self.r.send(KEEPER, &liq.abi_encode());
                }
                81 => {
                    let drain = drainDeferredWorkCall { marketId: m };
                    let _ = self.r.send(KEEPER, &drain.abi_encode());
                }
                82..=83 => self.fat_finger(u),
                84..=86 => self.family_trade(k, mk),
                87..=94 => self.ladder(k, mk),
                95..=97 => self.pull(m),
                _ => self.cancel_algo(u, m),
            }
        }

        /// A taker order against liquidity a provider rests next to the mark: a market order, an
        /// IOC limit through it, or a FOK (sometimes larger than what rests — refused).
        fn taker(&mut self, u: Address, k: usize, mk: u64) {
            let m = SPECS[k].id;
            let usd = self.rng.pick(&[2_000u64, 10_000, 30_000, 80_000]);
            let qty = self.rand_qty(k, usd);
            let side = self.rng.pick(&[BUY, SELL]);
            let lp = self.rng.pick(&LPS);
            let price = off(k, mk, self.rng.below(50), side == BUY);
            let lq = self.place(lp, m, 1 - side, price, qty, LIMIT, GTC, 0);
            match self.rng.below(10) {
                0..=4 => {
                    let _ = self.place(u, m, side, 0, qty, MKT, GTC, 0);
                }
                5..=7 => {
                    let through = off(k, price, 20, side == BUY);
                    let _ = self.place(u, m, side, through, qty, LIMIT, IOC, 0);
                }
                _ => {
                    let q = if self.rng.chance(1, 4) { qty + 1 + qty / 2 } else { qty };
                    let _ = self.place(u, m, side, price, q, LIMIT, FOK, 0);
                }
            }
            if let Some(id) = lq {
                if self.is_open(lp, m, id) {
                    let c = cancelOrderCall { orderId: id.into(), marketId: m };
                    let _ = self.r.send(lp, &c.abi_encode());
                }
            }
        }

        /// A resting limit away from the mark: GTC, PostOnly or reduce-only.
        fn limit(&mut self, u: Address, k: usize, mk: u64) {
            let m = SPECS[k].id;
            let side = self.rng.pick(&[BUY, SELL]);
            let bps = 10 + self.rng.below(500);
            let price = off(k, mk, bps, side == SELL);
            let qty = self.rand_qty(k, 20_000);
            let (tif, flags) = match self.rng.below(20) {
                0..=11 => (GTC, 0),
                12..=16 => (POST_ONLY, 0),
                _ => (GTC, RO),
            };
            let _ = self.place(u, m, side, price, qty, LIMIT, tif, flags);
        }

        /// A reduce-only close of the whole position: as taker into provider liquidity, or as maker
        /// at the mark taken by a provider.
        fn close(&mut self, u: Address, k: usize, mk: u64) {
            let m = SPECS[k].id;
            let a = self.amount(u, m);
            if a == 0 {
                return;
            }
            let side = if a > 0 { SELL } else { BUY };
            let q = a.unsigned_abs();
            let lp = self.rng.pick(&LPS);
            if self.rng.chance(1, 2) {
                let lq = self.place(lp, m, 1 - side, mk, q, LIMIT, GTC, 0);
                let _ = self.place(u, m, side, 0, q, MKT, GTC, RO);
                if let Some(id) = lq {
                    if self.is_open(lp, m, id) {
                        let c = cancelOrderCall { orderId: id.into(), marketId: m };
                        let _ = self.r.send(lp, &c.abi_encode());
                    }
                }
            } else if self.place(u, m, side, mk, q, LIMIT, GTC, RO).is_some() {
                let _ = self.place(lp, m, 1 - side, 0, q, MKT, GTC, 0);
            }
        }

        /// One of `u`'s orders cancelled, or all of them in one batch.
        fn cancels(&mut self, u: Address, m: u64) {
            let ids = self.open_orders(u, m);
            if ids.is_empty() {
                return;
            }
            if ids.len() > 1 && self.rng.chance(1, 3) {
                let ids = ids.iter().map(|i| (*i).into()).collect();
                let _ = self.call(u, batchCancelOrdersCall { orderIds: ids }.abi_encode());
            } else {
                let id = self.rng.pick(&ids);
                let c = cancelOrderCall { orderId: id.into(), marketId: m };
                let _ = self.call(u, c.abi_encode());
            }
        }

        /// Money out at the withdrawable amount or one unit past it; spot out; money in.
        fn money(&mut self, u: Address) {
            let is_sub = self.master_of(u).is_some();
            match self.rng.below(4) {
                0 => {
                    let x = self.r.view(&getAccountCall { user: u }).maxWithdrawAmount;
                    let amount = if self.rng.chance(2, 3) { (x / 3).max(1) } else { x + 1 };
                    let _ = self.call(u, transferFromPerpCall { amount }.abi_encode());
                }
                1 if !is_sub => {
                    let spot: u64 = self.spot(u);
                    if spot > 0 {
                        let amount = U256::from(spot / 2 + 1);
                        let _ = self.r.send(u, &withdrawCall { amount }.abi_encode());
                    }
                }
                2 if !is_sub => {
                    let held = self.erc20(u);
                    if held > 0 {
                        let amount = held / 4 + 1;
                        let dep = depositCall { amount: U256::from(amount) };
                        let _ = self.r.send(u, &dep.abi_encode());
                        let _ = self.r.send(u, &transferToPerpCall { amount }.abi_encode());
                    }
                }
                _ => {
                    let spot = self.spot(u);
                    let amount = (spot / 2).max(1);
                    let _ = self.call(u, transferToPerpCall { amount }.abi_encode());
                }
            }
        }

        /// A margin-type switch (mixed); in the isolated trace a redundant ISOLATED (refused
        /// `-4046`), which is what a client re-asserting its mode sends.
        fn switch_type(&mut self, u: Address, m: u64) {
            let t = match self.mode {
                Mode::Mixed => {
                    if self.margin_type(u, m) == CROSSED {
                        ISOLATED
                    } else {
                        CROSSED
                    }
                }
                Mode::Isolated => ISOLATED,
            };
            let _ = self.call(u, setMarginTypeCall { marketId: m, marginType: t }.abi_encode());
        }

        /// Isolated margin in or out, on a market where `u` holds an open position (the first one
        /// found from `m` on; ISOLATED or not — a CROSSED one is refused `-4049`).
        fn isolated_margin(&mut self, u: Address, m: u64) {
            let ks = self.ks.clone();
            let start = ks.iter().position(|&k| SPECS[k].id == m).unwrap();
            for i in 0..ks.len() {
                let mm = SPECS[ks[(start + i) % ks.len()]].id;
                if self.amount(u, mm) == 0 {
                    continue;
                }
                let amount = (1 + self.rng.below(500)) * USD;
                let input = if self.rng.chance(1, 2) {
                    addPositionMarginCall { marketId: mm, amount }.abi_encode()
                } else {
                    removePositionMarginCall { marketId: mm, amount: amount / 4 }.abi_encode()
                };
                let _ = self.call(u, input);
                return;
            }
        }

        /// A protective conditional on an open position, with provider liquidity a little through
        /// its trigger for it to fire into.
        fn conditional(&mut self, u: Address, k: usize, mk: u64) {
            let m = SPECS[k].id;
            let a = self.amount(u, m);
            if a == 0 {
                return;
            }
            let side = if a > 0 { SELL } else { BUY };
            let q = a.unsigned_abs();
            let bps = 50 + self.rng.below(600);
            // A stop sits on the losing side of the mark, a take-profit on the winning side.
            let stop_trigger = off(k, mk, bps, a < 0);
            let tp_trigger = off(k, mk, bps, a > 0);
            let (input, trigger) = match self.rng.below(5) {
                0 => (algo(m, side, 1, stop_trigger, 0, 0, CLOSE_POSITION), stop_trigger),
                1 => {
                    let part = 1 + self.rng.below(q);
                    (algo(m, side, 1, stop_trigger, 0, part, RO), stop_trigger)
                }
                2 => (algo(m, side, 3, tp_trigger, 0, 0, CLOSE_POSITION), tp_trigger),
                _ => {
                    let cb = 10 * (1 + self.rng.below(50));
                    (algo(m, side, 4, 0, cb, q, RO), stop_trigger)
                }
            };
            if self.call(u, input).is_ok() {
                let price = off(k, trigger, 150, side == BUY);
                let lp = self.rng.pick(&LPS);
                let _ = self.place(lp, m, 1 - side, price, q, LIMIT, GTC, 0);
            }
        }

        fn cancel_algo(&mut self, u: Address, m: u64) {
            let ids = self.r.view(&getOpenAlgoOrdersCall { user: u, marketId: m });
            if let Some(id) = ids.first() {
                let c = I::cancelAlgoOrderCall { algoId: *id, marketId: m };
                let _ = self.call(u, c.abi_encode());
            }
        }

        /// A batch of one to four placements over random markets.
        fn batch(&mut self, u: Address) {
            let mut items = Vec::new();
            for _ in 0..1 + self.rng.below(4) {
                let k = self.rng.pick(&self.ks.clone());
                let m = SPECS[k].id;
                let mk = self.mark(m);
                let side = self.rng.pick(&[BUY, SELL]);
                let q = self.rand_qty(k, 10_000);
                match self.rng.below(3) {
                    0 => {
                        let lp = self.rng.pick(&LPS);
                        let _ = self.place(lp, m, 1 - side, mk, q, LIMIT, GTC, 0);
                        items.push(item(m, side, 0, q, MKT, GTC, 0));
                    }
                    1 => {
                        let p = off(k, mk, 20 + self.rng.below(300), side == SELL);
                        items.push(item(m, side, p, q, LIMIT, POST_ONLY, 0));
                    }
                    _ => {
                        let p = off(k, mk, 20 + self.rng.below(300), side == SELL);
                        items.push(item(m, side, p, q, LIMIT, GTC, 0));
                    }
                }
            }
            let _ = self.call(u, batchPlaceOrdersCall { orders: items }.abi_encode());
        }

        /// A position closed as maker far through the mark (a fat finger): the realised loss can
        /// leave the wallet negative (CROSSED) or the silo short (ISOLATED).
        fn fat_finger(&mut self, u: Address) {
            for s in self.ks.clone().into_iter().map(|k| SPECS[k]) {
                let a = self.amount(u, s.id);
                if a == 0 {
                    continue;
                }
                let mk = self.mark(s.id);
                let k = SPECS.iter().position(|x| x.id == s.id).unwrap();
                let bps = 1_500 + self.rng.below(4_000);
                let side = if a > 0 { SELL } else { BUY };
                let price = off(k, mk, bps, a < 0);
                for id in self.open_orders(u, s.id) {
                    let c = cancelOrderCall { orderId: id.into(), marketId: s.id };
                    let _ = self.call(u, c.abi_encode());
                }
                let q = a.unsigned_abs();
                if self.place(u, s.id, side, price, q, LIMIT, GTC, RO).is_some() {
                    let lp = self.rng.pick(&LPS);
                    let _ = self.place(lp, s.id, 1 - side, 0, q, MKT, GTC, 0);
                }
                return;
            }
        }

        /// A master and its sub trade with each other, after an internal transfer.
        fn family_trade(&mut self, k: usize, mk: u64) {
            let m = SPECS[k].id;
            let (master, sub) = self.rng.pick(&self.subs.clone());
            let amount = (1 + self.rng.below(3_000)) * USD;
            let (to, from) = if self.rng.chance(1, 2) { (sub, master) } else { (master, sub) };
            let t =
                internalTransferCall { to, fromWallet: PERP_WALLET, toWallet: PERP_WALLET, amount };
            let _ = self.call(from, t.abi_encode());
            let q = self.rand_qty(k, 5_000);
            let side = self.rng.pick(&[BUY, SELL]);
            if let Some(id) = self.place(master, m, side, mk, q, LIMIT, GTC, 0) {
                let _ = self.place(sub, m, 1 - side, 0, q, MKT, GTC, 0);
                if self.is_open(master, m, id) {
                    let c = cancelOrderCall { orderId: id.into(), marketId: m };
                    let _ = self.r.send(master, &c.abi_encode());
                }
            }
        }

        /// LP1 replaces its standing ladder on one market: two levels a side.
        fn ladder(&mut self, k: usize, mk: u64) {
            let m = SPECS[k].id;
            self.pull(m);
            let mut items = Vec::new();
            for (i, bps) in [(0u64, 20 + self.rng.below(40)), (1, 80 + self.rng.below(120))] {
                let q = self.rand_qty(k, 40_000 + 40_000 * i);
                items.push(item(m, BUY, off(k, mk, bps, false), q, LIMIT, GTC, 0));
                let q = self.rand_qty(k, 40_000 + 40_000 * i);
                items.push(item(m, SELL, off(k, mk, bps, true), q, LIMIT, GTC, 0));
            }
            let _ = self.r.send(LP1, &batchPlaceOrdersCall { orders: items }.abi_encode());
        }

        /// LP1 pulls every quote it has on one market.
        fn pull(&mut self, m: u64) {
            let ids = self.open_orders(LP1, m);
            if !ids.is_empty() {
                let ids = ids.iter().map(|i| (*i).into()).collect();
                let _ = self.r.send(LP1, &batchCancelOrdersCall { orderIds: ids }.abi_encode());
            }
        }

        // ── the scenes ──

        /// Starts a block late enough that every market can tick again.
        fn fresh_block(&mut self) {
            let t = self.r.h.now - self.r.h.now % 15 + 15 + self.rng.below(5);
            self.r.next_block(t);
        }

        fn available(&mut self, u: Address) -> u64 {
            self.r.view(&getAccountCall { user: u }).availableBalance.max(0) as u64
        }

        /// `who` opens `side` worth `usd` dollars on market `k` against a provider's quote at the
        /// mark.
        fn open(&mut self, who: Address, k: usize, side: u8, usd: u64, lp: Address) -> u64 {
            let m = SPECS[k].id;
            let mk = self.mark(m);
            let q = self.qty_for(k, usd);
            self.place(lp, m, 1 - side, mk, q, LIMIT, GTC, 0).expect("the provider quotes");
            let input = order(m, side, 0, q, MKT, GTC, 0);
            self.call(who, input).unwrap_or_else(|e| panic!("{who} open on {m}: {e}"));
            q
        }

        /// The fat finger (books still empty): X3 buys about 2.5x its wallet on the last market in
        /// play (the SOL-shaped one when all three are), then closes it as maker 45 % under the
        /// mark. CROSSED (mixed): the realised loss
        /// leaves the wallet negative with no position left, and the insurance fund clears it at
        /// the end of the provider's call (Q8, reason 7). ISOLATED: the loss runs past the silo
        /// into bad debt.
        fn scene_fat_finger(&mut self) {
            let k = *self.ks.last().unwrap();
            let m = SPECS[k].id;
            self.fresh_block();
            let usd = (self.available(X3) / USD * 5 / 2).min(first_tier_cap(&SPECS[k]));
            let q = self.open(X3, k, BUY, usd, LP1);
            self.r.next_block(self.r.h.now + 3);
            let mk = self.mark(m);
            let price = off(k, mk, 4_500, false);
            self.place(X3, m, SELL, price, q, LIMIT, GTC, RO).expect("the close rests");
            self.place(LP2, m, BUY, 0, q, MKT, GTC, 0).expect("the provider takes it");
        }

        /// A crash on the BTC-shaped market: X1 (CROSSED in the mixed trace) long about 11x its
        /// available balance at 20x (inside the first tier), with a second leg and resting orders
        /// on the ETH-shaped market when it is in play; X4 (ISOLATED) long the same way, its silo
        /// topped up and half of that taken back. The provider bids 4 % and 6 % under the mark;
        /// the index drops 7 %. X1's liquidation cancels every order it has, X4's silo is
        /// liquidated.
        fn scene_crash(&mut self) {
            let (k, k2) = (self.ks[0], self.ks[1 % self.ks.len()]);
            let (m, m2) = (SPECS[k].id, SPECS[k2].id);
            self.fresh_block();
            let x1 = self.available(X1) / USD;
            if k2 != k {
                self.open(X1, k2, BUY, x1 / 5, LP1);
            }
            let q = self.open(X1, k, BUY, (x1 * 11).min(45_000), LP1);
            let x4 = self.available(X4) / USD;
            let q4 = self.open(X4, k, BUY, (x4 * 11).min(45_000), LP2);
            // X4 tops its silo up and takes half of it back out.
            let add = addPositionMarginCall { marketId: m, amount: 100 * USD };
            self.r.ok(X4, &add.abi_encode());
            let remove = removePositionMarginCall { marketId: m, amount: 50 * USD };
            self.r.ok(X4, &remove.abi_encode());
            let mk = self.mark(m);
            let mk2 = self.mark(m2);
            let _ = self.place(X1, m, BUY, off(k, mk, 1_500, false), 1, LIMIT, GTC, 0);
            if k2 != k {
                let _ = self.place(X1, m2, BUY, off(k2, mk2, 800, false), 1, LIMIT, GTC, 0);
            }
            for bps in [400, 600] {
                let price = off(k, mk, bps, false);
                let _ = self.place(LP1, m, BUY, price, (q + q4) / 4, LIMIT, GTC, 0);
            }
            self.fresh_block();
            let next = self.index[k] - self.index[k] * 7 / 100;
            assert!(self.tick(k, next), "the crash tick");
        }

        /// A spike on the last market in play (the SOL-shaped one at 3x when all three are) against
        /// X2, short about 2.7x its wallet (inside the first tier), after the provider pulled its
        /// quotes: the account is bankrupt, the book has little for the close, and the rest is
        /// ADL-ed against the longs in profit.
        fn scene_spike(&mut self) {
            let k = *self.ks.last().unwrap();
            let m = SPECS[k].id;
            self.fresh_block();
            let usd = (self.available(X2) / USD * 27 / 10).min(first_tier_cap(&SPECS[k]));
            self.open(X2, k, SELL, usd, LP2);
            self.pull(m);
            self.fresh_block();
            let next = self.index[k] + self.index[k] * 45 / 100;
            assert!(self.tick(k, next), "the spike tick");
        }

        /// The clock jumps past the next funding boundary and the oracle catches every market up;
        /// a trader takes some money out to its spot balance and off the venue.
        fn funding_jump(&mut self) {
            let t = (self.r.h.now / 3_600 + 1) * 3_600 + 5 + self.rng.below(40);
            self.r.next_block(t);
            for k in self.ks.clone() {
                let cur = self.index[k];
                let d = cur * self.rng.below(60) / 10_000;
                let next = if self.rng.chance(1, 2) { cur + d } else { cur - d };
                assert!(self.tick(k, next), "the catch-up tick");
            }
            let u = trader(1 + self.rng.below(6) as u8);
            let x = self.r.view(&getAccountCall { user: u }).maxWithdrawAmount;
            if x > 0 {
                let amount = (x / 4).max(1);
                self.r.ok(u, &transferFromPerpCall { amount }.abi_encode());
                self.r.ok(u, &withdrawCall { amount: U256::from(amount) }.abi_encode());
            }
        }

        /// The trace: the fat-finger scene on the empty books, then blocks of random steps until
        /// `target` calls, with the funding jumps at a third and two thirds, the crash at 40 % and
        /// the spike at 60 %, checkpointed at each quarter.
        pub(super) fn run(&mut self, target: usize) {
            let start = self.r.calls.len();
            self.scene_fat_finger();
            let mut next_cp = 1;
            let mut fired = [false; 4];
            while self.r.calls.len() < target {
                let done = (self.r.calls.len() - start) * 100 / (target - start);
                if done >= 33 && !fired[0] {
                    fired[0] = true;
                    self.funding_jump();
                } else if done >= 40 && !fired[1] {
                    fired[1] = true;
                    self.scene_crash();
                } else if done >= 60 && !fired[2] {
                    fired[2] = true;
                    self.scene_spike();
                } else if done >= 66 && !fired[3] {
                    fired[3] = true;
                    self.funding_jump();
                } else {
                    let dt = if self.rng.chance(1, 30) {
                        600 + self.rng.below(1_200)
                    } else {
                        2 + self.rng.below(4)
                    };
                    let t = self.r.h.now + dt;
                    self.r.next_block(t);
                    self.oracle_round();
                }
                for _ in 0..1 + self.rng.below(4) {
                    self.step();
                }
                let done = self.r.calls.len() - start;
                if next_cp < 4 && done * 4 >= (target - start) * next_cp {
                    let at = self.r.h.now;
                    self.r.checkpoint(&format!("trace {next_cp}/4 (t={at})"));
                    next_cp += 1;
                }
            }
            self.r.checkpoint("end");
        }
    }
}

/// `AccountBalanceChanged` rows of `user` with `reason`, as `totalCrossWalletBalance`s.
fn balance_rows(r: &Recorder, user: Address, reason: u8) -> Vec<i64> {
    let mut out = Vec::new();
    for c in &r.calls {
        for (_, l) in &c.logs {
            let t = l.data.topics();
            if t[0] == I::AccountBalanceChanged::SIGNATURE_HASH {
                let e = I::AccountBalanceChanged::decode_raw_log(t, &l.data.data).unwrap();
                if e.user == user && e.reason == reason {
                    out.push(e.totalCrossWalletBalance);
                }
            }
        }
    }
    out
}

/// What every random fixture must contain, whatever the mode.
fn assert_common_coverage(r: &Recorder, scenario: &str) {
    for key in [
        "Trade",
        "OrderRested",
        "OrderExpired",
        "OrderCancelled.reason=0",
        "OrderCancelled.reason=3",
        "Adl",
        "InsuranceFundChanged",
        "FundingRateComputed",
        "FundingSettled.amount!=0",
        "AlgoOrderPlaced",
        "AlgoOrderTriggered",
        "AlgoOrderActivated",
        "PositionMarginAdjusted",
        "SubAccountCreated",
        "InternalTransfer",
        "Deposit",
        "Withdraw",
        "TransferFromPerp",
        "Liquidation.marginType=1",
    ] {
        if count(r, key) == 0 {
            let (by_name, detail) = event_counts(&r.calls);
            panic!("{scenario}: no {key}\n  events {by_name:?}\n  detail {detail:?}");
        }
    }
}

fn random_fixture(mode: random_trace::Mode, ks: &[usize], seed: u64, target: usize) -> Recorder {
    let mut g = random_trace::Gen::new(seed, mode, ks);
    g.run(target);
    g.r
}

/// Nothing CROSSED ever moves: the shape of the harness's `MARGIN_MODE=isolated` runs.
fn assert_isolated_only(r: &Recorder, scenario: &str) {
    for key in [
        "CrossLiquidationStarted",
        "CrossLiquidationEnded",
        "Liquidation.marginType=0",
        "PositionChanged.marginType=0",
        "MarginTypeChanged.marginType=0",
    ] {
        assert_eq!(count(r, key), 0, "{scenario}: {key}");
    }
}

const ROUTES: &str = "Random blocks of market / IOC / FOK / limit / PostOnly / reduce-only \
     orders, batches, cancels (single and batch), leverage, money in and out, isolated margin in \
     and out, stop / take-profit / closePosition / trailing conditionals, keeper liquidate() and \
     drainDeferredWork, provider ladders and pulls, master / sub trades, oracle moves with gaps, \
     market-wide crashes and clock jumps.";

#[test]
#[ignore = "fixture exporter — PERP_FIXTURE_OUT=<dir> cargo test -p perp-engine --lib \
            fixture_export -- --ignored"]
fn fixture_export_mixed_random() {
    const SEED: u64 = 0x0c20_55ed_2026_1026;
    let r = random_fixture(random_trace::Mode::Mixed, &[0, 1, 2], SEED, 650);
    assert_common_coverage(&r, "mixed_random");
    for key in [
        "CrossLiquidationStarted",
        "Liquidation.marginType=0",
        "PositionChanged.marginType=0",
        "PositionChanged.marginType=1",
        "MarginTypeChanged.marginType=0",
        "MarginTypeChanged.marginType=1",
    ] {
        assert!(count(&r, key) > 0, "mixed_random: no {key}");
    }
    let q8 = balance_rows(&r, random_trace::X3, 7);
    assert_eq!(q8, vec![0], "mixed_random: X3's negative wallet cleared to 0 (Q8)");
    let description = format!(
        "Seeded random trace, MIXED margin types. Three markets shaped like the devnet's BTC / \
         ETH / SOL, all on 3 / 2 decimals: id 2 (20x / 10x / 5x tiers, 1.25% clearance fee, \
         hourly funding), id 4 (10x, a 10% price band, 0.5% fee, hourly negative-rate funding), \
         id 6 (3x, no funding, no fee). Two liquidity providers (LP1 ISOLATED, LP2 CROSSED), six \
         traders CROSSED by default with a third of their markets ISOLATED and switching both \
         ways, two sub-accounts trading through asSubAccount, maker-rebate and positive maker \
         rates. {ROUTES} Scenes: X3's fat-finger close leaves its CROSSED wallet negative with \
         no position and the insurance fund clears it (Q8, reason 7); funding epochs rolled at \
         1/3 and 2/3; a crash liquidates X1 (CROSSED: the episode, cancel-all across markets) \
         and X4 (ISOLATED) in one sweep; a spike makes X2 (CROSSED) bankrupt, ADL-ed."
    );
    emit(&r, "mixed_random", &description, Some(SEED));
}

#[test]
#[ignore = "fixture exporter — PERP_FIXTURE_OUT=<dir> cargo test -p perp-engine --lib \
            fixture_export -- --ignored"]
fn fixture_export_isolated_random() {
    const SEED: u64 = 0x0150_1a7e_2026_1026;
    let r = random_fixture(random_trace::Mode::Isolated, &[0, 1, 2], SEED, 650);
    assert_common_coverage(&r, "isolated_random");
    assert_isolated_only(&r, "isolated_random");
    let description = format!(
        "The mixed_random generator and markets with every trading actor (both providers, the \
         six traders, the two sub-accounts, the scene accounts) setMarginType(ISOLATED) on every \
         market before it trades, as the harness's MARGIN_MODE=isolated provisioning does; the \
         margin-type step re-sends ISOLATED (refused -4046). Admin, oracle and keeper never trade \
         and keep the default. {ROUTES} Scenes: X3's fat-finger close runs past its silo into \
         bad debt; funding epochs rolled at 1/3 and 2/3; a crash liquidates X1 and X4 \
         (cancel-all); a spike liquidates X2 with ADL. No CROSSED position ever moves."
    );
    emit(&r, "isolated_random", &description, Some(SEED));
}

/// (d) on market 2 alone — what the harness's single-market runs (`MARKET_ID` = 2) look like.
#[test]
#[ignore = "fixture exporter — PERP_FIXTURE_OUT=<dir> cargo test -p perp-engine --lib \
            fixture_export -- --ignored"]
fn fixture_export_isolated_random_m2() {
    const SEED: u64 = 0x0150_1a7e_0002_1026;
    let r = random_fixture(random_trace::Mode::Isolated, &[0], SEED, 650);
    assert_common_coverage(&r, "isolated_random_m2");
    assert_isolated_only(&r, "isolated_random_m2");
    let description = format!(
        "isolated_random on ONE market, id 2 (the BTC-shaped one: 3 / 2 decimals, 20x / 10x / 5x \
         tiers, 1.25% clearance fee, hourly funding), which is what the harness's single-market \
         runs (MARKET_ID = 2) model end to end. Every trading actor ISOLATED. {ROUTES} Scenes, \
         all on market 2: X3's fat-finger close into bad debt, funding epochs at 1/3 and 2/3, a \
         crash liquidating X1 and X4, a spike liquidating X2 with ADL."
    );
    emit(&r, "isolated_random_m2", &description, Some(SEED));
}

// ── (e) cross scenes: the shapes a random trace reaches rarely or never ────────────────────────

/// Deterministic scenes the harness review asked for, each one a shape a random trace reaches by
/// luck or not at all:
///
/// * a lazily-deleted id in a level's FIFO (an owner cancel of an order that is not first in its
///   queue), seen by a checkpoint's `getBookLevel` while its `getOrder` reverts;
/// * a CROSSED loser ADL-ed at its bankruptcy price against a CROSSED winner that is already under
///   water (fallback tier: the fund covers its fill down to its `E`), in the same call as the
///   loser's capped clearance fee and the winner's negative-wallet clearance (Q8) — once with the
///   loser's address below the winner's and once above, since the end-of-call drain publishes
///   their reason-7 rows in address order;
/// * a CROSSED maker refused on a wallet the fund has cleared to 0 (no cash at hand), and a maker
///   refused because the fill would carry its leverage past its tier (both reason 2).
mod cross_scenes {
    use super::*;

    const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    const ORACLE: Address = address!("0000000000000000000000000000000000000c1e");
    /// Deep liquidity, ISOLATED on every market.
    const LP: Address = address!("7777777777777777777777777777777777777777");
    /// The first ADL scene: the loser's address is BELOW the winner's.
    const L1: Address = address!("1111111111111111111111111111111111111111");
    const W1: Address = address!("2222222222222222222222222222222222222222");
    /// The second: the winner's address is below the loser's.
    const W2: Address = address!("5555555555555555555555555555555555555555");
    const L2: Address = address!("6666666666666666666666666666666666666666");
    /// The maker refused with no cash, and the maker refused at its tier.
    const C: Address = address!("3333333333333333333333333333333333333333");
    const D: Address = address!("4444444444444444444444444444444444444444");
    const TRADERS: [Address; 6] = [L1, W1, W2, L2, C, D];
    /// The two ADL markets: a 20% band, one 20x tier.
    const M20: u64 = 20;
    const M23: u64 = 23;
    /// Wide band, one 20x tier: the winners' second legs, the cash-less maker, the stale id.
    const M21: u64 = 21;
    /// Wide band, 20x below $5k and 5x above: the tier refusal.
    const M22: u64 = 22;
    const MARKETS: [u64; 4] = [M20, M21, M22, M23];
    const USD: u64 = 1_000_000;
    const BUY: u8 = 0;
    const SELL: u8 = 1;
    const GTC: u8 = 0;
    const RO: u8 = 0b01;
    const ISOLATED: u8 = 1;
    const T0: u64 = 1_800;

    /// Every market prices in cents (`priceDecimals` 2) and trades whole units.
    const fn px(dollars: u64) -> u64 {
        dollars * 100
    }

    fn limit(m: u64, side: u8, price: u64, qty: u64, flags: u8) -> Vec<u8> {
        let c = placeOrderCall {
            marketId: m,
            side,
            price,
            quantity: qty,
            orderType: 0,
            tif: GTC,
            clientOrderId: FixedBytes::default(),
            flags,
        };
        c.abi_encode()
    }

    fn market(m: u64, side: u8, qty: u64) -> Vec<u8> {
        let c = placeOrderCall {
            marketId: m,
            side,
            price: 0,
            quantity: qty,
            orderType: 1,
            tif: GTC,
            clientOrderId: FixedBytes::default(),
            flags: 0,
        };
        c.abi_encode()
    }

    fn tick(r: &mut Recorder, m: u64, dollars: u64) {
        let ts = r.h.now - r.h.now % 15;
        let u = updateIndexPriceCall { marketId: m, indexPrice: px(dollars), timestamp: ts };
        r.ok(ORACLE, &u.abi_encode());
    }

    fn last_id(r: &Recorder) -> Id {
        r.last_ret[..32].try_into().unwrap()
    }

    /// The ADL scene on `m`: `loser` short 150 at $100 against `winner`'s long; `winner` then
    /// loses $3,600 on a reduce-only fat-finger of a second long (M21), which leaves it at
    /// `E = −$2,600`. A spike to $116 takes the loser to `E = −$1,400` and the winner up to
    /// `−$200`: the sweep closes the loser at its bankruptcy price clamped into the band ($106.66,
    /// no book), ADL-ing the winner — fallback tier, under water — whose fill the fund covers back
    /// up to `−$200`; the loser's clearance fee is capped at its last dollar of equity, and the
    /// winner, flat with a negative wallet, is cleared by the fund at the end of the call.
    fn adl_scene(r: &mut Recorder, m: u64, loser: Address, winner: Address, t: u64) {
        r.next_block(t);
        // The loser rests first, so it is ahead of the winner in the market's registry.
        r.ok(loser, &limit(m, SELL, px(100), 150, 0));
        r.ok(winner, &market(m, BUY, 150));
        r.ok(LP, &limit(M21, SELL, px(100), 40, 0));
        r.ok(winner, &market(M21, BUY, 40));
        r.next_block(t + 3);
        r.ok(winner, &limit(M21, SELL, px(10), 40, RO));
        r.ok(LP, &market(M21, BUY, 40));
        let e = r.view(&getAccountCall { user: winner }).crossMarginBalance;
        assert_eq!(e, -2_600 * USD as i64, "the winner is under water before the spike");
        r.next_block(t + 15);
        tick(r, m, 116);
        let w = r.view(&getAccountCall { user: winner });
        assert_eq!((w.totalCrossWalletBalance, w.positions.len()), (0, 0), "Q8 cleared the winner");
        assert_eq!(r.view(&getPositionCall { user: loser, marketId: m }).amount, 0);
    }

    pub(super) fn scenario() -> Recorder {
        let mut rec = Recorder::new(T0, 1);
        let r = &mut rec;
        for (u, label) in [
            (ADMIN, "admin"),
            (ORACLE, "oracle"),
            (LP, "lp"),
            (L1, "l1_loser"),
            (W1, "w1_underwater_winner"),
            (W2, "w2_underwater_winner"),
            (L2, "l2_loser"),
            (C, "c_cashless_maker"),
            (D, "d_tier_capped_maker"),
        ] {
            r.actor(u, label);
        }
        r.markets = MARKETS.to_vec();
        r.mint(ADMIN, 2_000_000 * USD);
        r.mint(LP, 10_000_000 * USD);
        for u in TRADERS {
            r.mint(u, 1_000 * USD);
        }
        r.ok(ADMIN, &initAdminCall { admin: ADMIN }.abi_encode());
        r.ok(ADMIN, &setOracleAddressCall { oracle: ORACLE }.abi_encode());
        for m in MARKETS {
            let band = if m == M20 || m == M23 { 2_000 } else { 1_000_000 };
            let add = addMarketCall {
                marketId: m,
                baseDecimals: 0,
                priceDecimals: 2,
                tickSize: 1,
                stepSize: 1,
                minQuantity: 1,
                maxQuantity: 1_000_000,
                maxPrice: 100_000_000,
                priceUpdateInterval: 15,
                fundingInterval: 0,
                interestRate: 0,
                liquidationFeeRateBps: 125,
                initialMarkPrice: px(100),
                priceBandBps: band,
            };
            r.ok(ADMIN, &add.abi_encode());
            let (lower_bounds, max_leverages) =
                if m == M22 { (vec![0, 5_000 * USD], vec![20, 5]) } else { (vec![0], vec![20]) };
            let tiers = setMarginTiersCall {
                marketId: m,
                maintMarginRatesPpm: vec![0; lower_bounds.len()],
                lowerBounds: lower_bounds,
                maxLeverages: max_leverages,
                notionalCap: 0,
            };
            r.ok(ADMIN, &tiers.abi_encode());
        }
        r.ok(ADMIN, &depositCall { amount: U256::from(1_000_000 * USD) }.abi_encode());
        r.ok(ADMIN, &transferToPerpCall { amount: 500_000 * USD }.abi_encode());
        r.ok(ADMIN, &depositInsuranceFundCall { amount: 100_000 * USD }.abi_encode());
        r.ok(LP, &depositCall { amount: U256::from(5_000_000 * USD) }.abi_encode());
        r.ok(LP, &transferToPerpCall { amount: 5_000_000 * USD }.abi_encode());
        for u in TRADERS {
            r.ok(u, &depositCall { amount: U256::from(1_000 * USD) }.abi_encode());
            r.ok(u, &transferToPerpCall { amount: 1_000 * USD }.abi_encode());
        }
        for m in MARKETS {
            r.ok(LP, &setMarginTypeCall { marketId: m, marginType: ISOLATED }.abi_encode());
            for u in TRADERS {
                r.ok(u, &setLeverageCall { marketId: m, leverage: 20 }.abi_encode());
            }
        }
        r.checkpoint("setup: roles, markets, money in, every trader CROSSED at 20x");

        // ── A lazily-deleted id: the second of two orders at one price, cancelled by its owner,
        //    stays in the level's FIFO (only the live count drops) until a walk sweeps it ──
        r.next_block(T0 + 3);
        r.ok(LP, &limit(M21, SELL, px(500), 1, 0));
        let first = last_id(r);
        r.ok(LP, &limit(M21, SELL, px(500), 1, 0));
        let second = last_id(r);
        r.ok(LP, &cancelOrderCall { orderId: second.into(), marketId: M21 }.abi_encode());
        let level = r.view(&getBookLevelCall { marketId: M21, side: SELL, price: px(500) });
        let ids: Vec<Id> = level.iter().map(|x| x.0).collect();
        assert_eq!(ids, vec![first, second], "the cancelled id stays in the FIFO");
        let get = getOrderCall { orderId: second.into(), marketId: M21 }.abi_encode();
        let (reverted, _) = r.probe(|h| static_call(h, &get));
        assert!(reverted, "getOrder of the stale id reverts");
        r.checkpoint("a stale id in the M21 ask level at $500");

        adl_scene(r, M20, L1, W1, T0 + 15);
        r.checkpoint("ADL of L1 against the under-water W1 (loser's address below)");
        adl_scene(r, M23, L2, W2, T0 + 45);
        r.checkpoint("ADL of L2 against the under-water W2 (winner's address below)");

        // ── A CROSSED maker whose wallet the fund cleared: its opening bid is refused (2) ──
        r.next_block(T0 + 75);
        r.ok(C, &limit(M21, BUY, px(90), 10, 0));
        r.ok(LP, &limit(M22, SELL, px(100), 40, 0));
        r.ok(C, &market(M22, BUY, 40));
        r.ok(C, &limit(M22, SELL, px(10), 40, RO));
        r.ok(LP, &market(M22, BUY, 40));
        let c = r.view(&getAccountCall { user: C });
        assert_eq!(c.totalCrossWalletBalance, 0, "Q8 cleared C (its bid holds no position)");
        // The walk reaches C's bid, refuses it, finds nothing else and expires the remainder.
        r.ok(LP, &market(M21, SELL, 10));
        assert_eq!(r.view(&getPositionCall { user: C, marketId: M21 }).amount, 0);

        // ── A maker whose fill would carry its leverage past its tier: refused (2) ──
        r.next_block(T0 + 90);
        r.ok(D, &limit(M22, BUY, px(100), 80, 0));
        r.ok(LP, &market(M22, SELL, 80));
        let d = r.view(&getPositionCall { user: D, marketId: M22 });
        assert_eq!(d.amount, 0, "D's bid was refused, not filled");
        r.checkpoint("end");
        rec
    }
}

#[test]
#[ignore = "fixture exporter — PERP_FIXTURE_OUT=<dir> cargo test -p perp-engine --lib \
            fixture_export -- --ignored"]
fn fixture_export_cross_scenes() {
    let r = cross_scenes::scenario();
    for (key, at_least) in [
        ("CrossLiquidationStarted", 2),
        ("Adl", 2),
        ("OrderCancelled.reason=2", 2),
        ("AccountBalanceChanged.reason=7", 6),
    ] {
        assert!(count(&r, key) >= at_least, "cross_scenes: {key} < {at_least}");
    }
    emit(
        &r,
        "cross_scenes",
        "Deterministic scenes on four markets (ids 20-23, 0 / 2 decimals, $100 marks): a stale \
         id in a level's FIFO (an owner cancel of the second order at a price) under a \
         checkpoint; twice, a CROSSED loser ADL-ed at its band-clamped bankruptcy price against \
         an under-water CROSSED winner (fallback tier, the fund covering the fill), with the \
         loser's clearance fee capped at its last dollar and the winner's negative wallet \
         cleared (Q8) in the same call - first with the loser's address below the winner's, \
         then above; a CROSSED maker refused on the wallet the fund cleared to 0; a maker \
         refused at its leverage tier.",
        None,
    );
}

// ── (f) cross episodes: stale legs, the flag, and the close order ──────────────────────────────

/// The account-level episode's own decisions, each in a call of its own:
///
/// * F (CROSSED on M24 and M25) is caught by a crash on M24 while M25's mark is stale (no tick for
///   longer than three oracle intervals, Q10): the episode closes M24 and leaves M25 alone, so the
///   flag stays. F's opening order is refused (-2023); its reduce-only one on M25 rests, a provider
///   takes it (R1: a flagged maker's reduce-only order fills), and with no cross position left the
///   flag ends at the end of that call.
/// * H (CROSSED on M24, ISOLATED on M26) is caught by the same crash and recovers in one step;
///   later its ISOLATED M26 position is liquidated next to its cross wallet.
/// * E (CROSSED on M27, M28 and M29, each fresh) is made bankrupt by a crash on M27: the episode
///   closes ALL three in one call, in its documented order — the trigger market first, then by
///   maintenance margin DESCENDING (M29 before M28, against market-id order).
mod cross_episodes {
    use super::*;

    const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    const ORACLE: Address = address!("0000000000000000000000000000000000000c1e");
    /// Deep liquidity, ISOLATED on every market.
    const LP: Address = address!("7777777777777777777777777777777777777777");
    const F: Address = address!("1111111111111111111111111111111111111111");
    const G: Address = address!("2222222222222222222222222222222222222222");
    const H: Address = address!("3333333333333333333333333333333333333333");
    const E: Address = address!("4444444444444444444444444444444444444444");
    const TRADERS: [Address; 4] = [F, G, H, E];
    /// Hourly-ish funding (every 60 s, 300 ppm) so the episode's funding settles move money.
    const M24: u64 = 24;
    const M25: u64 = 25;
    const M26: u64 = 26;
    /// No funding: the close-order scene.
    const M27: u64 = 27;
    const M28: u64 = 28;
    const M29: u64 = 29;
    const MARKETS: [u64; 6] = [M24, M25, M26, M27, M28, M29];
    const USD: u64 = 1_000_000;
    const BUY: u8 = 0;
    const SELL: u8 = 1;
    const GTC: u8 = 0;
    const RO: u8 = 0b01;
    const ISOLATED: u8 = 1;
    const T0: u64 = 3_600;

    /// Every market prices in cents (`priceDecimals` 2) and trades whole units.
    const fn px(dollars: u64) -> u64 {
        dollars * 100
    }

    fn limit(m: u64, side: u8, price: u64, qty: u64, flags: u8) -> Vec<u8> {
        let c = placeOrderCall {
            marketId: m,
            side,
            price,
            quantity: qty,
            orderType: 0,
            tif: GTC,
            clientOrderId: FixedBytes::default(),
            flags,
        };
        c.abi_encode()
    }

    fn market(m: u64, side: u8, qty: u64) -> Vec<u8> {
        let c = placeOrderCall {
            marketId: m,
            side,
            price: 0,
            quantity: qty,
            orderType: 1,
            tif: GTC,
            clientOrderId: FixedBytes::default(),
            flags: 0,
        };
        c.abi_encode()
    }

    fn tick(r: &mut Recorder, m: u64, cents: u64) -> Vec<Log> {
        let ts = r.h.now - r.h.now % 15;
        let u = updateIndexPriceCall { marketId: m, indexPrice: cents, timestamp: ts };
        r.ok(ORACLE, &u.abi_encode())
    }

    /// `marketId` of every CROSSED `Liquidation` of `user` in `logs`, in log order.
    fn cross_steps(logs: &[Log], user: Address) -> Vec<u64> {
        let mut out = Vec::new();
        for l in logs {
            let t = l.data.topics();
            if t[0] == I::Liquidation::SIGNATURE_HASH {
                let e = I::Liquidation::decode_raw_log(t, &l.data.data).unwrap();
                if e.user == user && e.marginType == 0 {
                    out.push(e.marketId);
                }
            }
        }
        out
    }

    fn flagged(logs: &[Log], user: Address, started: bool) -> bool {
        logs.iter().any(|l| {
            let t = l.data.topics();
            let hash = if started {
                I::CrossLiquidationStarted::SIGNATURE_HASH
            } else {
                I::CrossLiquidationEnded::SIGNATURE_HASH
            };
            t[0] == hash && Address::from_word(t[1]) == user
        })
    }

    pub(super) fn scenario() -> Recorder {
        let mut rec = Recorder::new(T0, 1);
        let r = &mut rec;
        for (u, label) in [
            (ADMIN, "admin"),
            (ORACLE, "oracle"),
            (LP, "lp"),
            (F, "f_stale_leg"),
            (G, "g"),
            (H, "h_cross_and_isolated"),
            (E, "e_three_legs"),
        ] {
            r.actor(u, label);
        }
        r.markets = MARKETS.to_vec();
        r.mint(ADMIN, 2_000_000 * USD);
        r.mint(LP, 10_000_000 * USD);
        for u in TRADERS {
            r.mint(u, 2_000 * USD);
        }
        r.ok(ADMIN, &initAdminCall { admin: ADMIN }.abi_encode());
        r.ok(ADMIN, &setOracleAddressCall { oracle: ORACLE }.abi_encode());
        for m in MARKETS {
            let funded = m <= M26;
            let add = addMarketCall {
                marketId: m,
                baseDecimals: 0,
                priceDecimals: 2,
                tickSize: 1,
                stepSize: 1,
                minQuantity: 1,
                maxQuantity: 1_000_000,
                maxPrice: 100_000_000,
                priceUpdateInterval: 15,
                fundingInterval: if funded { 60 } else { 0 },
                interestRate: if funded { 300 } else { 0 },
                liquidationFeeRateBps: 100,
                initialMarkPrice: px(100),
                priceBandBps: 1_000_000,
            };
            r.ok(ADMIN, &add.abi_encode());
            let tiers = setMarginTiersCall {
                marketId: m,
                lowerBounds: vec![0],
                maxLeverages: vec![20],
                maintMarginRatesPpm: vec![0],
                notionalCap: 0,
            };
            r.ok(ADMIN, &tiers.abi_encode());
        }
        r.ok(ADMIN, &depositCall { amount: U256::from(1_000_000 * USD) }.abi_encode());
        r.ok(ADMIN, &transferToPerpCall { amount: 500_000 * USD }.abi_encode());
        r.ok(ADMIN, &depositInsuranceFundCall { amount: 100_000 * USD }.abi_encode());
        r.ok(LP, &depositCall { amount: U256::from(5_000_000 * USD) }.abi_encode());
        r.ok(LP, &transferToPerpCall { amount: 5_000_000 * USD }.abi_encode());
        for u in TRADERS {
            r.ok(u, &depositCall { amount: U256::from(2_000 * USD) }.abi_encode());
            r.ok(u, &transferToPerpCall { amount: 2_000 * USD }.abi_encode());
        }
        for m in MARKETS {
            r.ok(LP, &setMarginTypeCall { marketId: m, marginType: ISOLATED }.abi_encode());
            for u in TRADERS {
                r.ok(u, &setLeverageCall { marketId: m, leverage: 20 }.abi_encode());
            }
        }
        // H: ISOLATED on M26 (an isolated leg next to its cross wallet).
        r.ok(H, &setMarginTypeCall { marketId: M26, marginType: ISOLATED }.abi_encode());
        for m in [M24, M25, M26] {
            tick(r, m, px(100));
        }
        r.checkpoint("setup: roles, markets, money in, traders CROSSED at 20x, H ISOLATED on M26");

        // ── F: CROSSED on M24 and M25; G and H on M24 too; H ISOLATED on M26 ──
        r.next_block(T0 + 5);
        r.ok(LP, &limit(M24, SELL, px(100), 200, 0));
        r.ok(F, &market(M24, BUY, 200));
        r.ok(LP, &limit(M25, SELL, px(100), 150, 0));
        r.ok(F, &market(M25, BUY, 150));
        r.ok(LP, &limit(M24, SELL, px(100), 100, 0));
        r.ok(G, &market(M24, BUY, 100));
        r.ok(LP, &limit(M24, SELL, px(100), 100, 0));
        r.ok(H, &market(M24, BUY, 100));
        r.ok(LP, &limit(M26, SELL, px(100), 200, 0));
        r.ok(H, &market(M26, BUY, 200));
        // A funding epoch on M24-M26.
        r.next_block(T0 + 61);
        for m in [M24, M25, M26] {
            tick(r, m, px(100));
        }
        r.checkpoint("legs open, a funding epoch rolled");

        // ── The crash on M24 while M25's mark ages past three intervals ──
        r.next_block(T0 + 100);
        tick(r, M24, px(100));
        tick(r, M26, px(100));
        r.ok(LP, &limit(M24, BUY, px(89), 60, 0));
        r.next_block(T0 + 130);
        let crash = tick(r, M24, px(88));
        assert!(flagged(&crash, F, true), "F's episode starts");
        assert_eq!(cross_steps(&crash, F), vec![M24], "F's stale M25 leg is not closed");
        assert!(!flagged(&crash, F, false), "F stays flagged (its M25 leg is left)");
        r.checkpoint("the M24 crash: F flagged with its stale M25 leg, H back to health");

        // F may only reduce: an opening order is refused (-2023); its reduce-only sell rests at the
        // stored mark and a provider takes it, which leaves no cross position: the flag ends.
        r.next_block(T0 + 135);
        r.refused(F, &limit(M25, SELL, px(100), 10, 0), "2023");
        r.ok(F, &limit(M25, SELL, px(100), 150, RO));
        let take = r.ok(LP, &market(M25, BUY, 150));
        assert!(flagged(&take, F, false), "F's flag ends with its last cross position");
        // H: its ISOLATED M26 long liquidated into a bid, next to its cross wallet.
        r.next_block(T0 + 145);
        r.ok(LP, &limit(M26, BUY, px(92), 300, 0));
        tick(r, M26, 9_350);
        assert_eq!(r.view(&getPositionCall { user: H, marketId: M26 }).amount, 0);
        r.checkpoint("F's flag ended by a reduce-only fill; H's isolated M26 liquidated");

        // ── E: three fresh CROSSED legs, made bankrupt by a crash on one of them ──
        // MM = N / 40 per leg: M27 $10,000 → 250, M28 $3,000 → 75, M29 $6,000 → 150.
        r.next_block(T0 + 160);
        for (m, qty) in [(M27, 100), (M28, 30), (M29, 60)] {
            r.ok(LP, &limit(m, SELL, px(100), qty, 0));
            r.ok(E, &market(m, BUY, qty));
        }
        r.next_block(T0 + 175);
        tick(r, M28, px(100));
        tick(r, M29, px(100));
        r.next_block(T0 + 180);
        let crash = tick(r, M27, px(70));
        assert_eq!(
            cross_steps(&crash, E),
            vec![M27, M29, M28],
            "the trigger market first, then maintenance margin descending"
        );
        assert!(flagged(&crash, E, false), "E's episode ends with no cross position");
        r.checkpoint("end");
        rec
    }
}

#[test]
#[ignore = "fixture exporter — PERP_FIXTURE_OUT=<dir> cargo test -p perp-engine --lib \
            fixture_export -- --ignored"]
fn fixture_export_cross_episodes() {
    let r = cross_episodes::scenario();
    for (key, at_least) in [
        ("CrossLiquidationStarted", 3),
        ("CrossLiquidationEnded", 3),
        ("Liquidation.marginType=0", 5),
        ("Liquidation.marginType=1", 1),
        ("FundingSettled.amount!=0", 1),
    ] {
        assert!(count(&r, key) >= at_least, "cross_episodes: {key} < {at_least}");
    }
    emit(
        &r,
        "cross_episodes",
        "Deterministic cross-episode scenes on six markets (ids 24-29, 0 / 2 decimals, $100 \
         marks; 24-26 roll funding every 60 s): a crash on M24 catches F while its M25 mark is \
         stale, so the episode closes M24 only and the flag stays; F's opening order is refused \
         (-2023), its reduce-only sell rests and is taken, and the flag ends with its last cross \
         position; H recovers in one step, and later its ISOLATED M26 position is liquidated next \
         to its cross wallet; E's three fresh cross legs (M27, M28, M29) are all closed in one \
         call after a crash on M27 - the trigger market first, then by maintenance margin \
         descending (M29 before M28).",
        None,
    );
}
