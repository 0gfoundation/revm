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
