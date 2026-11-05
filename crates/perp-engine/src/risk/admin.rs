//! Admin roles: the admin's ownership, the market-manager role and the oracle address.

use super::*;

// ── Admin: ownership ──────────────────────────────────────────────────────────

/// `initAdmin(address admin)` — one-time initialisation; fails if already set.
pub fn run_init_admin<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = initAdminCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("initAdmin: invalid calldata"))?;

    if args.admin == Address::ZERO {
        return Err(perp_err("initAdmin: admin cannot be zero address"));
    }
    let current = storage::load_admin(context)?;
    if current != Address::ZERO {
        return Err(perp_err("initAdmin: admin already initialised"));
    }
    storage::save_admin(context, args.admin)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::AdminInitialized { admin: args.admin }.to_log_data(),
    });
    Ok(Bytes::new())
}

/// `transferAdmin(address newAdmin)` — only callable by current admin.
pub fn run_transfer_admin<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = transferAdminCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("transferAdmin: invalid calldata"))?;

    if args.newAdmin == Address::ZERO {
        return Err(perp_err("transferAdmin: new admin cannot be zero address"));
    }
    let current = storage::load_admin(context)?;
    if current == Address::ZERO {
        return Err(perp_err("transferAdmin: admin not yet initialised"));
    }
    if caller != current {
        return Err(perp_err("transferAdmin: caller is not admin"));
    }
    storage::save_admin(context, args.newAdmin)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::AdminTransferred {
            previousAdmin: current,
            newAdmin: args.newAdmin,
        }
        .to_log_data(),
    });
    Ok(Bytes::new())
}

/// `getAdmin() returns (address admin)`
pub fn run_get_admin<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    getAdminCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAdmin: invalid calldata"))?;
    let admin = storage::load_admin(context)?;
    Ok(Bytes::from(getAdminCall::abi_encode_returns(&admin)))
}

// ── Market manager role ───────────────────────────────────────────────────────

/// `setMarketManagerAddress(address manager)` — admin only. Zero revokes.
pub fn run_set_market_manager<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setMarketManagerAddressCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setMarketManagerAddress: invalid calldata"))?;

    require_admin(caller, context)?;

    let previous = storage::load_market_manager(context)?;
    storage::save_market_manager(context, args.manager)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarketManagerUpdated {
            previousManager: previous,
            newManager: args.manager,
        }
        .to_log_data(),
    });
    Ok(Bytes::new())
}

/// `getMarketManagerAddress() returns (address manager)`
pub fn run_get_market_manager<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    getMarketManagerAddressCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarketManagerAddress: invalid calldata"))?;
    let manager = storage::load_market_manager(context)?;
    Ok(Bytes::from(
        getMarketManagerAddressCall::abi_encode_returns(&manager),
    ))
}

// ── Oracle address ────────────────────────────────────────────────────────────

/// `setOracleAddress(address oracle)` — admin only.
pub fn run_set_oracle_address<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setOracleAddressCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setOracleAddress: invalid calldata"))?;

    require_admin(caller, context)?;

    let previous = storage::load_oracle(context)?;
    storage::save_oracle(context, args.oracle)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OracleAddressUpdated {
            previousOracle: previous,
            newOracle: args.oracle,
        }
        .to_log_data(),
    });
    Ok(Bytes::new())
}

/// `getOracleAddress() returns (address oracle)`
pub fn run_get_oracle_address<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    getOracleAddressCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getOracleAddress: invalid calldata"))?;
    let oracle = storage::load_oracle(context)?;
    Ok(Bytes::from(getOracleAddressCall::abi_encode_returns(
        &oracle,
    )))
}
