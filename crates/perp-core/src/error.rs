//! Engine-level error type.
//!
//! Mirrors the two `PrecompileError` shapes the perp code actually uses: a clean,
//! user-attributable reject (surfaced as an ABI-encoded revert by the precompile shell)
//! and a fatal invariant violation (must halt — in-memory state is unsafe to continue).
//! The shell converts via its `From<PerpError> for PrecompileError` impl, so engine code
//! keeps using plain `?`.

/// PerpDEX engine error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PerpError {
    /// Clean reject: invalid input, insufficient funds, business-rule violation.
    /// Becomes a REVERT with an `Error(string)` payload at the precompile boundary.
    Reject(String),
    /// Fatal invariant violation: accounting failure that makes the in-memory state
    /// unsafe to continue executing. Propagates as a halting error, never a revert.
    Fatal(String),
}

impl core::fmt::Display for PerpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PerpError::Reject(m) => f.write_str(m),
            PerpError::Fatal(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for PerpError {}

/// Convenience constructor for clean rejects (same call shape as the old `perp_err`).
pub fn perp_err(msg: impl Into<String>) -> PerpError {
    PerpError::Reject(msg.into())
}

/// Invariant violation: a condition that should be impossible under correct system
/// operation — indicates a bug in the matching engine or state management. Still a
/// reject at the boundary; tests treat the `[INVARIANT]` prefix as a hard failure.
pub fn perp_invariant_err(msg: impl Into<String>) -> PerpError {
    PerpError::Reject(format!("[INVARIANT] {}", msg.into()))
}

/// Fatal invariant violation (accounting failure): halts instead of reverting.
pub fn perp_fatal_invariant_err(msg: impl Into<String>) -> PerpError {
    PerpError::Fatal(format!("[INVARIANT] {}", msg.into()))
}
