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
    /// A clean reject that follows writes the path DELIBERATELY retains — the only sanctioned
    /// exception to validate-then-apply, and today its only user is the signed-call replay burn.
    ///
    /// Identical to [`PerpError::Reject`] at the boundary: same revert, same reason string. The
    /// difference is `sanctioned_writes`, which the commit-only #23 guard in the call shell
    /// subtracts before deciding whether the call leaked state under a failed receipt.
    ///
    /// ⚠️ It is an ALLOWANCE, not a pardon. Measure it at the retained write itself
    /// (`count_after − count_before`) and pass exactly that; any write the *rejecting* code made on
    /// top of it still trips the guard. Do not hardcode a number — the burn's write count varies
    /// with what its GC sweep happens to reclaim, and a hardcoded slack would silently cover a real
    /// leak the day the sweep writes less than expected.
    ///
    /// Only sound when the surviving state is what the caller SHOULD observe despite the failure.
    /// "your signature was spent" qualifies; "your margin was debited" does not.
    RejectAfterRetainedWrite {
        /// Revert reason, verbatim — same string `Reject` would have carried.
        message: String,
        /// How many perp writes before this reject were deliberate and are meant to survive.
        sanctioned_writes: u64,
    },
    /// Fatal invariant violation: accounting failure that makes the in-memory state
    /// unsafe to continue executing. Propagates as a halting error, never a revert.
    Fatal(String),
    /// Call-shell error: the selector's flat gas exceeds the provided gas limit.
    OutOfGas,
    /// Call-shell error: a state-mutating selector was invoked in a static context.
    StaticRestrictionViolation,
    /// Call-shell error: missing/unknown selector or malformed stateful-call input.
    StatefulInvalidInput,
}

impl core::fmt::Display for PerpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PerpError::Reject(m) => f.write_str(m),
            PerpError::RejectAfterRetainedWrite { message, .. } => f.write_str(message),
            PerpError::Fatal(m) => f.write_str(m),
            PerpError::OutOfGas => f.write_str("out of gas"),
            PerpError::StaticRestrictionViolation => f.write_str("static restriction violated"),
            PerpError::StatefulInvalidInput => f.write_str("invalid stateful-call input"),
        }
    }
}

impl std::error::Error for PerpError {}

impl PerpError {
    /// Tag a reject as following `sanctioned_writes` deliberately-retained perp writes, so the
    /// commit-only #23 guard does not read them as a leak. See
    /// [`PerpError::RejectAfterRetainedWrite`] for when that is legitimate — and for why the count
    /// must be measured rather than assumed.
    ///
    /// `Fatal` and the shell errors pass through unchanged: they do not become reverts, so the
    /// guard never sees them.
    pub fn after_retained_writes(self, sanctioned_writes: u64) -> Self {
        match self {
            PerpError::Reject(message) => PerpError::RejectAfterRetainedWrite {
                message,
                sanctioned_writes,
            },
            other => other,
        }
    }
}

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
