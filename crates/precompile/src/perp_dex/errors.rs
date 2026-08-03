use crate::PrecompileError;

/// Convenience constructor for precompile errors in the perp DEX.
pub fn perp_err(msg: impl Into<String>) -> PrecompileError {
    PrecompileError::Other(msg.into())
}

/// Invariant violation: a condition that should be impossible under correct system operation.
/// Errors with this prefix indicate a bug in the matching engine or state management.
/// Tests should treat any occurrence as a hard failure distinct from normal user errors.
pub fn perp_invariant_err(msg: impl Into<String>) -> PrecompileError {
    PrecompileError::Other(format!("[INVARIANT] {}", msg.into()))
}

/// Fatal invariant violation. Use this for accounting failures that make the
/// in-memory state unsafe to continue executing.
pub fn perp_fatal_invariant_err(msg: impl Into<String>) -> PrecompileError {
    PrecompileError::Fatal(format!("[INVARIANT] {}", msg.into()))
}

/// Boundary conversion: engine-core errors surface as the same two `PrecompileError`
/// shapes the perp code has always used (`Other` = clean revert, `Fatal` = halt), so
/// engine calls compose with `?` in shell code unchanged.
impl From<perp_core::PerpError> for crate::PrecompileError {
    fn from(e: perp_core::PerpError) -> Self {
        match e {
            perp_core::PerpError::Reject(m) => crate::PrecompileError::Other(m),
            perp_core::PerpError::Fatal(m) => crate::PrecompileError::Fatal(m),
            perp_core::PerpError::OutOfGas => crate::PrecompileError::OutOfGas,
            perp_core::PerpError::StaticRestrictionViolation => {
                crate::PrecompileError::StaticRestrictionViolation
            }
            perp_core::PerpError::StatefulInvalidInput => {
                crate::PrecompileError::StatefulInvalidInput
            }
        }
    }
}
