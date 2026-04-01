use crate::PrecompileError;

/// Convenience constructor for precompile errors in the perp DEX.
pub fn perp_err(msg: impl Into<String>) -> PrecompileError {
    PrecompileError::Other(msg.into())
}
