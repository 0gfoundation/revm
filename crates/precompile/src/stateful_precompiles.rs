//! stateful precompile execution
use ::bytecode::Bytecode;
use context::{ContextTr, Database, JournalTr};
use primitives::{bytes, Address, U256};

use crate::{
    da_signers::{run_da_signers_call, DA_SIGNERS_ADDRESS},
    wa0gi_base::{run_wa0gi_base_call, WA0GI_BASE_ADDRESS},
    PrecompileError, PrecompileResult,
};

/// run stateful precompile
pub fn run_stateful_precompile<CTX: ContextTr>(
    to: Address,
    input_bytes: &[u8],
    gas_limit: u64,
    reservoir: u64,
    caller: Address,
    value: U256,
    is_static: bool,
    context: &mut CTX,
) -> PrecompileResult {
    // check input length
    if input_bytes.len() < 4 {
        return Err(PrecompileError::Other("Invalid input length".to_string()));
    }
    // set init code
    if !is_static &&
        context.journal_mut().code(to).map_err(convert_db_err::<CTX::Db>)?.data.is_empty()
    {
        context.journal_mut().set_code(to, Bytecode::new_legacy(bytes!("01")))
    }

    let result = match to {
        DA_SIGNERS_ADDRESS => {
            run_da_signers_call(input_bytes, gas_limit, caller, value, is_static, context)
        }
        WA0GI_BASE_ADDRESS => {
            run_wa0gi_base_call(input_bytes, gas_limit, caller, value, is_static, context)
        }
        _ => Err(PrecompileError::Other(format!("Stateful precompile {to:?} not found"))),
    };

    match result {
        Ok(mut output) => {
            output.reservoir = reservoir;
            Ok(output)
        }
        Err(PrecompileError::Fatal(message)) => Err(PrecompileError::Fatal(message)),
        Err(PrecompileError::FatalAny(error)) => Err(PrecompileError::FatalAny(error)),
        Err(error) => {
            let halt = if matches!(error, PrecompileError::OutOfGas) {
                crate::PrecompileHalt::OutOfGas
            } else {
                crate::PrecompileHalt::other(error.to_string())
            };
            Ok(crate::PrecompileOutput::halt(halt, reservoir))
        }
    }
}

/// convert error type
pub fn convert_db_err<Db: Database>(e: Db::Error) -> PrecompileError {
    PrecompileError::Other(format!("Storage Error: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        contract_interface::WrappedA0GIBase::{minterSupplyCall, setMinterCapCall, Supply},
        PrecompileStatus,
    };
    use alloy_sol_types::SolCall;
    use context::{BlockEnv, CfgEnv, Context, JournalTr, TxEnv};
    use database::InMemoryDB;
    use primitives::hardfork::SpecId;

    #[test]
    fn wa0gi_state_persists_and_static_mutation_halts() {
        let minter = Address::with_last_byte(0x44);
        let mut context: Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB> =
            Context::new(InMemoryDB::default(), SpecId::default());
        let set_cap =
            setMinterCapCall { minter, cap: U256::from(500), initialSupply: U256::from(120) }
                .abi_encode();

        let static_output = run_stateful_precompile(
            WA0GI_BASE_ADDRESS,
            &set_cap,
            100_000,
            0,
            crate::wa0gi_base::WA0GI_AGENCY_ADDRESS,
            U256::ZERO,
            true,
            &mut context,
        )
        .unwrap();
        assert!(matches!(static_output.status, PrecompileStatus::Halt(_)));

        let output = run_stateful_precompile(
            WA0GI_BASE_ADDRESS,
            &set_cap,
            100_000,
            17,
            crate::wa0gi_base::WA0GI_AGENCY_ADDRESS,
            U256::ZERO,
            false,
            &mut context,
        )
        .unwrap();
        assert!(output.is_success());
        assert_eq!(output.gas_used, 100_000);
        assert_eq!(output.reservoir, 17);
        assert!(!context.journal_mut().code(WA0GI_BASE_ADDRESS).unwrap().is_empty());

        let query = minterSupplyCall { minter }.abi_encode();
        let output = run_stateful_precompile(
            WA0GI_BASE_ADDRESS,
            &query,
            10_000,
            0,
            Address::ZERO,
            U256::ZERO,
            true,
            &mut context,
        )
        .unwrap();
        assert_eq!(
            output.bytes,
            minterSupplyCall::abi_encode_returns(&Supply {
                cap: U256::from(500),
                initialSupply: U256::from(120),
                supply: U256::from(120),
            })
        );
    }
}
