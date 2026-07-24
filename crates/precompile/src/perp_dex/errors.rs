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
