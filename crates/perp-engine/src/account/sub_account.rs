//! Sub-accounts (P1): Hyperliquid-style DERIVED-ADDRESS sub-accounts, the `asSubAccount` wrapper a
//! master acts through, and `internalTransfer` between accounts.
//!
//! # The model
//!
//! A sub-account is an ordinary engine account keyed by an address nobody holds a key for:
//! `types::derive_sub_account(master, index)`. Because no one can sign as it, it can only act
//! through its master, via [`run_as_sub_account`] — which runs one whitelisted direct selector with
//! `caller := subAccount` through the SAME dispatch every direct call goes through. Everything a sub
//! owns (positions, orders, API keys, nonces, both balances) is keyed by its own address, so it is
//! isolated from its master and its siblings by construction: no handler below `dispatch` knows
//! sub-accounts exist.
//!
//! The family is two appended `UserAccount` fields and no new key prefix: `master` on the sub,
//! `sub_count` on the master (subs are `derive(master, i)` for `i < sub_count`).
//!
//! Every handler here is validate-then-apply (commit-only #23): every reject precedes the first
//! write, and the only `?` after it is a DB-error that aborts the block.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use primitives::{Address, Bytes, Log, U256};

use crate::{
    errors::perp_err,
    host::PerpHost,
    interface::IPerpDex::{
        self, addPositionMarginCall, asSubAccountCall, batchCancelOrdersCall,
        batchPlaceOrdersCall, cancelAlgoOrderCall, cancelOrderCall, createSubAccountCall,
        depositCall, getMasterCall, getSubAccountsCall, internalTransferCall, placeAlgoOrderCall,
        placeOrderCall, registerApiKeyCall, removePositionMarginCall, revokeApiKeyCall,
        setLeverageCall, transferFromPerpCall, transferToPerpCall, withdrawCall,
    },
    math::QUOTE_DECIMALS,
    storage,
    types::{
        derive_sub_account, AccountUpdateReason, UserAccount, MAX_PERP_WALLET_BALANCE,
        MAX_SUB_ACCOUNTS,
    },
    PerpError, PERP_DEX_ADDRESS,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// `internalTransfer` wallet code: the SPOT balance (`UserAccount::usdc_balance`, the
/// deposit/withdraw layer).
pub const WALLET_SPOT: u8 = 0;
/// `internalTransfer` wallet code: the PERP wallet (`UserAccount::perp_wallet_balance`).
pub const WALLET_PERP: u8 = 1;

/// Smallest EXTERNAL (outside-the-family) transfer: 1 USDC, i.e. `10^QUOTE_DECIMALS` quote units.
/// A spam floor on the one transfer shape that writes a stranger's account; family transfers have
/// none (they move a user's own money between their own accounts).
pub const MIN_EXTERNAL_TRANSFER: u64 = 10u64.pow(QUOTE_DECIMALS);
const _: () = assert!(MIN_EXTERNAL_TRANSFER == 1_000_000, "1 USDC at 6 dp");

/// Longest `createSubAccount` name, in BYTES (UTF-8). The name is a label in the
/// `SubAccountCreated` event only — never stored — so the cap only bounds the log payload; 32 bytes
/// is one ABI word and comfortably fits a human label. Empty is rejected: a nameless sub is
/// indistinguishable from its siblings in every UI that renders the directory.
pub const MAX_SUB_ACCOUNT_NAME_LEN: usize = 32;

/// Fixed gas `asSubAccount` adds on top of its INNER selector's own cost. It buys the wrapper's
/// own work: decoding the envelope, ONE account `_ref` load (the sub's `master`) and a whitelist
/// probe — the same shape as the 5_000 scalar-getter tier (`getMaster`, `getUserFeeRates`). This is
/// also the selector's entry in the `SELECTORS` table, i.e. the floor charged when the envelope is
/// undecodable or the inner selector is refused (the call then reverts having done no inner work).
pub const AS_SUB_ACCOUNT_OVERHEAD_GAS: u64 = 5_000;

// ── The asSubAccount whitelist ────────────────────────────────────────────────

/// **May a master run `selector` AS one of its sub-accounts?** Default-DENY: only the selectors
/// listed here, each a DIRECT, per-CALLER, mutating entry point whose every effect is keyed by the
/// caller — so with `caller := subAccount` the effect lands on the sub and nowhere else.
///
/// | allowed | why it is safe to run as the sub |
/// |---|---|
/// | `placeOrder` | order, nonce, position and margin are all keyed by `caller`; the order id is `keccak(sub ‖ sub's nonce)`, so two family members can never collide |
/// | `cancelOrder` | ownership is checked against `caller`, so only the sub's own orders can be cancelled |
/// | `batchPlaceOrders` | the batch driver's single owner is `caller`; gas includes its per-item cost |
/// | `batchCancelOrders` | same owner rule as `cancelOrder`, per item |
/// | `placeAlgoOrder` | conditional order keyed by `caller`, id from the sub's nonce |
/// | `cancelAlgoOrder` | ownership checked against `caller` |
/// | `setLeverage` | the sub's own position in that market |
/// | `addPositionMargin` | sub's perp wallet → sub's position silo |
/// | `removePositionMargin` | sub's position silo → sub's perp wallet, under the sub's own cap |
/// | `transferToPerp` | sub's spot → sub's perp wallet |
/// | `transferFromPerp` | sub's perp wallet → sub's spot, on the sub's own derived-available gate |
/// | `registerApiKey` | decision 3: keys are PER SUB — this is the ONLY way a sub can get one |
/// | `revokeApiKey` | the sub's own key slot |
/// | `internalTransfer` | `from := sub`; the family / external rules in [`run_internal_transfer`] then decide |
///
/// Everything else is refused, and each class is refused for a reason, not by omission:
///
/// * `deposit` / `withdraw` — decision 4: a sub's money enters and leaves through its master
///   (`internalTransfer`). Their ERC-20 legs pull from / pay to `caller`, and a derived address has
///   no key to ever spend ERC-20, so a `withdraw` would strand USDC at a keyless address. ⚠️ The
///   denial is not free on the other side: anyone CAN ERC-20-transfer USDC to a sub address (say,
///   as an exchange withdrawal destination), and `deposit` — which debits `caller`'s ERC-20, so it
///   would be a pure rescue path with no stranding risk — is the only thing that could sweep it in.
///   With it denied, such USDC is unrecoverable; clients must never present a sub address as an
///   ERC-20 destination. (Whitelisting `deposit` alone would close that; it is an owner call.)
/// * `createSubAccount` — no nesting.
/// * `asSubAccount` — no recursion (it would also re-enter this wrapper's gas accounting).
/// * every `*Signed` selector — they authenticate an explicit `account` argument by signature and
///   ignore `caller` entirely; wrapping them would add nothing but a second, weaker auth story. A
///   sub's API key already signs for the sub directly.
/// * every admin / role / oracle / market-manager / market-config / insurance-fund selector — a
///   role is held by an address, and a sub must never be able to inherit or exercise one.
/// * `liquidate`, `drainDeferredWork`, `updateIndexPriceFromBook` — permissionless or role-gated
///   protocol work that is not per-user; the master can call the permissionless ones directly.
/// * every view — pointless: views take the account as an argument and work on a sub unchanged.
pub fn sub_account_selector_allowed(selector: [u8; 4]) -> bool {
    [
        placeOrderCall::SELECTOR,
        cancelOrderCall::SELECTOR,
        batchPlaceOrdersCall::SELECTOR,
        batchCancelOrdersCall::SELECTOR,
        placeAlgoOrderCall::SELECTOR,
        cancelAlgoOrderCall::SELECTOR,
        setLeverageCall::SELECTOR,
        addPositionMarginCall::SELECTOR,
        removePositionMarginCall::SELECTOR,
        transferToPerpCall::SELECTOR,
        transferFromPerpCall::SELECTOR,
        registerApiKeyCall::SELECTOR,
        revokeApiKeyCall::SELECTOR,
        internalTransferCall::SELECTOR,
    ]
    .contains(&selector)
}

/// The up-front gas of an `asSubAccount` call: [`AS_SUB_ACCOUNT_OVERHEAD_GAS`] + the INNER
/// selector's own charge — its flat table cost, or, for an inner batch, the very
/// `BASE_BATCH_GAS + N * unit` [`crate::batch::batch_dynamic_gas`] would bill it directly. `None`
/// for every other selector.
///
/// Computed PRE-DISPATCH, before any write, exactly like the batch bound in `run_perp_dex_call`, so
/// an insufficient gas limit is a clean `OutOfGas` with zero writes. Pure: an ABI decode of the
/// envelope (the inner `bytes` is copied, bounded by the calldata length — the length word is
/// validated against the buffer before the copy) and two table lookups.
///
/// `None` is also returned when the envelope cannot be decoded or the inner selector is not
/// whitelisted: then only the overhead floor is charged and [`run_as_sub_account`] turns the same
/// condition into a clean revert. (Same convention as an over-cap batch.)
///
/// ⚠️ The charge is CALLDATA-ONLY, exactly like a batch's (whose items all failing on state still
/// pays `N * unit`): the floor applies only to what the calldata alone proves cannot run. A call
/// whose AUTHORIZATION fails — a non-master, or a master naming the wrong sub (an off-by-one index)
/// — is billed the full inner charge + overhead, then reverts. Only the caller pays, so it is not
/// exploitable; a read-only `master(sub) == caller` probe here could bill such a call the floor
/// instead (deterministic and write-free), at the cost of making the charge state-dependent.
pub fn as_sub_account_dynamic_gas(selector: [u8; 4], input: &[u8]) -> Option<u64> {
    if selector != asSubAccountCall::SELECTOR {
        return None;
    }
    let args = asSubAccountCall::abi_decode_validate(input).ok()?;
    let inner: [u8; 4] = args.data.get(..4)?.try_into().ok()?;
    if !sub_account_selector_allowed(inner) {
        return None;
    }
    let &(inner_table_gas, _) = crate::call::selectors_map().get(&inner)?;
    let inner_gas = crate::batch::batch_dynamic_gas(inner, &args.data).unwrap_or(inner_table_gas);
    Some(AS_SUB_ACCOUNT_OVERHEAD_GAS.saturating_add(inner_gas))
}

// ── createSubAccount ──────────────────────────────────────────────────────────

/// `createSubAccount(string name) returns (address subAccount)` — see the ABI doc.
pub fn run_create_sub_account<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = createSubAccountCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("createSubAccount: invalid calldata"))?;

    // ── VALIDATE (commit-only #23: every reject before the first write) ───────────────────
    if args.name.is_empty() {
        return Err(perp_err("createSubAccount: name must not be empty"));
    }
    if args.name.len() > MAX_SUB_ACCOUNT_NAME_LEN {
        return Err(perp_err(format!(
            "createSubAccount: name exceeds {MAX_SUB_ACCOUNT_NAME_LEN} bytes"
        )));
    }

    let (index, maker_fee_ppm, taker_fee_ppm) = {
        let me = storage::load_account_ref(context, caller)?;
        if me.master.is_some() {
            return Err(perp_err(
                "createSubAccount: a sub-account cannot create sub-accounts",
            ));
        }
        if me.sub_count >= MAX_SUB_ACCOUNTS {
            return Err(perp_err(format!(
                "createSubAccount: sub-account limit reached (max {MAX_SUB_ACCOUNTS})"
            )));
        }
        (me.sub_count, me.maker_fee_ppm, me.taker_fee_ppm)
    };
    let sub = derive_sub_account(caller, index);
    ensure_derived_address_unclaimed(context, sub)?;

    // ── APPLY (no logic reject past this point) ───────────────────────────────────────────
    // Both writes are `mutate_account` — the SILENT account writer — on purpose: neither moves a
    // balance (the family fields and the fee pair are not in `AccountBalanceChanged`'s payload), so
    // there is no truthful `reason` to publish, exactly like the nonce bump and `setUserFeeRates`.
    storage::mutate_account(context, sub, |a| {
        a.master = Some(caller);
        // Fee rates are inherited at birth and kept in step by `setUserFeeRates`' push-down.
        a.maker_fee_ppm = maker_fee_ppm;
        a.taker_fee_ppm = taker_fee_ppm;
    })?;
    storage::mutate_account(context, caller, |a| a.sub_count = index + 1)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::SubAccountCreated {
            master: caller,
            subAccount: sub,
            index,
            name: args.name,
        }
        .to_log_data(),
    });

    Ok(Bytes::from(createSubAccountCall::abi_encode_returns(&sub)))
}

/// The guard on the derived address: refuse to adopt one that already carries state only its own
/// KEY could have produced. That would mean someone controls it — which takes a keccak preimage,
/// i.e. never — but the check is a handful of resident loads, so it is cheap insurance.
///
/// # ⚠️ What it deliberately TOLERATES, and why
///
/// * **`usdc_balance`.** An external `internalTransfer` (SPOT → SPOT, ≥ 1 USDC) can credit ANY
///   ordinary address, and the next derived address of every master is public
///   (`derive(master, getSubAccounts(master).length)`). Refusing a non-zero spot balance would let
///   anyone permanently block a master's next sub-account for 1 USDC — the index is sequential, so
///   a blocked index blocks every later one too. A gifted balance is harmless: it becomes the new
///   sub's money, i.e. the master's. The consequence, documented on the ABI: a sub can be BORN
///   holding third-party money — the "no third party → sub" transfer rule binds only once the sub
///   exists, and the external path cannot tell a future sub from any other address (one-way hash).
/// * **`maker_fee_ppm` / `taker_fee_ppm`.** Admin-settable on any non-sub address, and overwritten
///   by the inherited rates below anyway.
///
/// Everything else on the account — a balance in the perp wallet or a silo, a nonce, a family link —
/// plus the per-user market index and the API-key id list, can only be produced by acting AS the
/// address, so any of it refuses the create. (A leverage-only flat position blob is the one such
/// state not covered: it is per market and there is no per-user list of them to read. It is
/// equally unreachable without the address's key.)
fn ensure_derived_address_unclaimed<H: PerpHost>(
    context: &mut H,
    sub: Address,
) -> Result<(), PerpError> {
    let claimed = {
        let a = storage::load_account_ref(context, sub)?;
        a.master.is_some()
            || a.sub_count != 0
            || a.perp_wallet_balance != 0
            || a.total_position_margin != 0
            || a.nonce != 0
    };
    if claimed
        || !storage::load_user_markets_ref(context, sub)?.is_empty()
        || !storage::load_api_key_ids(context, sub)?.is_empty()
    {
        return Err(perp_err(
            "createSubAccount: derived sub-account address already has engine state",
        ));
    }
    Ok(())
}

// ── asSubAccount ──────────────────────────────────────────────────────────────

/// `asSubAccount(address subAccount, bytes data)` — run ONE whitelisted inner call with
/// `caller := subAccount`.
///
/// The inner call goes through [`crate::call::dispatch`] — the exact handler table a direct call
/// reaches — so a sub's `placeOrder` IS `placeOrder`, not a re-implementation. What stays with the
/// OUTER call in `run_perp_dex_call` is everything that must happen once per call: the gas charge
/// (inner cost + overhead, see [`as_sub_account_dynamic_gas`]), the static check, the EOA depth
/// guard, the commit-only write tripwire, and the `AccountBalanceChanged` snapshot open/flush. The
/// inner call therefore cannot re-open or drain the snapshot set early, and its marks (the sub's)
/// drain with the outer call's, in address order.
///
/// Every reject here precedes the inner dispatch, so they are all write-free; the inner handler's own
/// validate-then-apply covers the rest. The inner return data AND the inner error are passed
/// through UNCHANGED.
pub fn run_as_sub_account<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = asSubAccountCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("asSubAccount: invalid calldata"))?;
    let sub = args.subAccount;

    // Authorization first, so a stranger learns nothing about the inner selector's fate.
    if storage::load_account_ref(context, sub)?.master != Some(caller) {
        return Err(perp_err(
            "asSubAccount: caller is not the master of subAccount",
        ));
    }

    let inner: [u8; 4] = args
        .data
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| perp_err("asSubAccount: inner call has no selector"))?;
    if inner == depositCall::SELECTOR || inner == withdrawCall::SELECTOR {
        return Err(perp_err(
            "asSubAccount: a sub-account cannot deposit or withdraw \
             (move funds through its master)",
        ));
    }
    if !sub_account_selector_allowed(inner) {
        return Err(perp_err(
            "asSubAccount: inner selector is not allowed for a sub-account",
        ));
    }

    crate::call::dispatch(inner, &args.data, sub, context)
}

// ── internalTransfer ──────────────────────────────────────────────────────────

/// `internalTransfer(address to, uint8 fromWallet, uint8 toWallet, uint64 amount)` — see the ABI
/// doc for the family / external rules.
///
/// Effects are off-trie ledger moves only (no ERC-20 leg): one debit and one credit of the same
/// `amount`, so `Σ usdc_balance + Σ perp_wallet_balance` is unchanged by construction. Each account
/// touched is written through `save_account` with [`AccountUpdateReason::InternalTransfer`], which
/// marks it for the end-of-call drain — so the stream is `InternalTransfer`, then one
/// `AccountBalanceChanged` per account in ASCENDING ADDRESS ORDER (the drain's PR#6 rule), both
/// zero-position groups. A `from == to` move touches one account and publishes one row.
pub fn run_internal_transfer<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = internalTransferCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("internalTransfer: invalid calldata"))?;
    let from = caller;
    let to = args.to;
    let (from_wallet, to_wallet, amount) = (args.fromWallet, args.toWallet, args.amount);

    // ── VALIDATE (commit-only #23: every reject before the first write) ───────────────────
    if from_wallet > WALLET_PERP || to_wallet > WALLET_PERP {
        return Err(perp_err(
            "internalTransfer: unknown wallet (0 = SPOT, 1 = PERP)",
        ));
    }
    if amount == 0 {
        return Err(perp_err("internalTransfer: amount must be > 0"));
    }
    if to == Address::ZERO {
        return Err(perp_err("internalTransfer: recipient cannot be the zero address"));
    }
    if to == PERP_DEX_ADDRESS {
        return Err(perp_err("internalTransfer: recipient cannot be the PerpDEX itself"));
    }
    // The USDC token contract can never call the PerpDEX, so a credit to it is stranded for good —
    // and pasting the token address where a recipient belongs is the easiest client mistake there
    // is. A keyless address IN GENERAL cannot be detected (a derived sub index ≥
    // `MAX_SUB_ACCOUNTS` is one), so nothing broader is attempted.
    if to == crate::USDC_ADDRESS {
        return Err(perp_err(
            "internalTransfer: recipient is the USDC token contract, which can never use its balance",
        ));
    }
    if from == to && from_wallet == to_wallet {
        return Err(perp_err(
            "internalTransfer: source and destination are the same wallet",
        ));
    }

    // Owned copies: both legs are computed on them before anything is written.
    let mut from_acct = storage::load_account(context, from)?;
    let mut to_acct = if from == to {
        None
    } else {
        Some(storage::load_account(context, to)?)
    };
    let to_master = to_acct.as_ref().map_or(from_acct.master, |a| a.master);

    let same_family = from_acct.family_root(from) == to_master.unwrap_or(to);
    if !same_family {
        // EXTERNAL. Binance parity: a sub never deals with a third party directly, in either
        // direction — funds leave and enter a family through its master.
        if from_acct.master.is_some() {
            return Err(perp_err(
                "internalTransfer: a sub-account cannot transfer outside its family",
            ));
        }
        if to_master.is_some() {
            return Err(perp_err(
                "internalTransfer: recipient is a sub-account of another family",
            ));
        }
        if from_wallet != WALLET_SPOT || to_wallet != WALLET_SPOT {
            return Err(perp_err(
                "internalTransfer: an external transfer must be SPOT to SPOT",
            ));
        }
        if amount < MIN_EXTERNAL_TRANSFER {
            return Err(perp_err(
                "internalTransfer: external transfer below the 1 USDC minimum",
            ));
        }
    }

    // Source gate.
    if from_wallet == WALLET_SPOT {
        let spot: U256 = from_acct.usdc_balance.clone().into();
        if spot < U256::from(amount) {
            return Err(perp_err("internalTransfer: insufficient spot balance"));
        }
    } else {
        // THE money-out gate of the perp wallet, identical to `transferFromPerp`'s: derived
        // `available = perp_wallet_balance − Σ ooIM`, recomputed fresh (never cached — it moves
        // with every market's mark). A positive `amount` must be covered in full.
        let available = crate::margin_view::derived_available_balance(context, from)?;
        if !crate::margin_view::derived_can_afford(available, amount as i128) {
            return Err(perp_err(
                "internalTransfer: insufficient perp wallet balance",
            ));
        }
    }

    // Both legs, in memory. Their arithmetic is fallible (destination caps), and that is still
    // pre-write.
    debit_wallet(&mut from_acct, from_wallet, amount)?;
    credit_wallet(to_acct.as_mut().unwrap_or(&mut from_acct), to_wallet, amount)?;

    // ── APPLY (no logic reject past this point; only DB-error `?`, which aborts the block) ──
    storage::save_account(context, from, from_acct, AccountUpdateReason::InternalTransfer)?;
    if let Some(to_acct) = to_acct {
        storage::save_account(context, to, to_acct, AccountUpdateReason::InternalTransfer)?;
    }

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::InternalTransfer {
            from,
            to,
            fromWallet: from_wallet,
            toWallet: to_wallet,
            amount,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// Debits `amount` from one of `acct`'s wallets. The caller has already run the source gate.
fn debit_wallet(acct: &mut UserAccount, wallet: u8, amount: u64) -> Result<(), PerpError> {
    if wallet == WALLET_SPOT {
        let spot: U256 = acct.usdc_balance.clone().into();
        let rest = spot
            .checked_sub(U256::from(amount))
            .ok_or_else(|| perp_err("internalTransfer: insufficient spot balance"))?;
        acct.usdc_balance = rest.into();
        Ok(())
    } else {
        acct.debit_perp(amount)
    }
}

/// Credits `amount` to one of `acct`'s wallets, refusing a result above the same `i64::MAX` cap
/// `deposit` enforces on the spot balance (and `credit_perp` on the perp wallet).
fn credit_wallet(acct: &mut UserAccount, wallet: u8, amount: u64) -> Result<(), PerpError> {
    if wallet == WALLET_SPOT {
        let spot: U256 = acct.usdc_balance.clone().into();
        let new = spot + U256::from(amount);
        if new > U256::from(MAX_PERP_WALLET_BALANCE) {
            return Err(perp_err(
                "internalTransfer: destination balance would exceed i64::MAX",
            ));
        }
        acct.usdc_balance = new.into();
        Ok(())
    } else {
        acct.credit_perp(amount)
    }
}

// ── Views ─────────────────────────────────────────────────────────────────────

/// `getSubAccounts(address master) returns (address[] subs)` — derived from the stored count, so
/// `subs[i] == derive_sub_account(master, i)`. Empty for a sub or an account that never created one.
pub fn run_get_sub_accounts<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getSubAccountsCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getSubAccounts: invalid calldata"))?;
    let n = storage::load_account_ref(context, args.master)?.sub_count;
    let subs: Vec<Address> = (0..n)
        .map(|i| derive_sub_account(args.master, i))
        .collect();
    Ok(Bytes::from(getSubAccountsCall::abi_encode_returns(&subs)))
}

/// `getMaster(address account) returns (address master)` — zero if `account` is not a sub.
pub fn run_get_master<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getMasterCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMaster: invalid calldata"))?;
    let master = storage::load_account_ref(context, args.account)?
        .master
        .unwrap_or(Address::ZERO);
    Ok(Bytes::from(getMasterCall::abi_encode_returns(&master)))
}

#[cfg(test)]
#[path = "sub_account_tests.rs"]
mod tests;

/// The spec §9 suite, written independently from the spec rather than from this implementation.
#[cfg(test)]
#[path = "sub_account_spec_tests.rs"]
mod spec_tests;
