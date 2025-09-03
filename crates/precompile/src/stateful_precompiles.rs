//! stateful precompile execution
use ::bytecode::{Bytecode, LegacyRawBytecode};
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
    if !is_static
        && context
            .journal_mut()
            .code(to)
            .map_err(convert_db_err::<CTX::Db>)?
            .data
            .is_empty()
    {
        context.journal_mut().set_code(
            to,
            Bytecode::LegacyAnalyzed(LegacyRawBytecode::into_analyzed(bytes!("0x01").into())),
        )
    }

    match to {
        DA_SIGNERS_ADDRESS => {
            run_da_signers_call(input_bytes, gas_limit, caller, value, is_static, context)
        }
        WA0GI_BASE_ADDRESS => {
            run_wa0gi_base_call(input_bytes, gas_limit, caller, value, is_static, context)
        }
        _ => Err(PrecompileError::Other(format!(
            "Stateful precompile {to:?} not found"
        ))),
    }
}

/// convert error type
pub fn convert_db_err<Db: Database>(e: Db::Error) -> PrecompileError {
    PrecompileError::Other(format!("Storage Error: {e:?}"))
}
