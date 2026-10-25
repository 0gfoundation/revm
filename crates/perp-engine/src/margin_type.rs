//! Margin type — Binance's `marginType` (CROSSED / ISOLATED), cross-margin series P1.
//!
//! # What exists, and what does not yet
//!
//! Every position record carries the owner's STORED choice ([`PerpPosition::margin_type`]); this
//! module owns the rule that turns it into the type IN FORCE ([`effective_margin_type`]). Cross
//! accounting (P2) and cross liquidation (P3) do not exist yet, so the rule is GATED: while
//! [`CROSS_MARGIN_ENABLED`] is `false` every position is ISOLATED in force, and every margin rule,
//! view and event behaves exactly as it did before the field existed. The gate flips as the last
//! step of P3.
//!
//! Every reader of the type goes through [`effective_margin_type`] — the `AccountPosition` row,
//! `PositionChanged` and `getSymbolConfig` today — so the gate lives in one place and the three
//! surfaces cannot disagree.

use crate::types::{MarginType, PerpPosition, CROSS_MARGIN_ENABLED};

/// Whether cross margin is live: [`CROSS_MARGIN_ENABLED`], a plain `const` in every non-test
/// build. Under `cfg(test)` a test may enable it for its own thread ([`test_gate::enable`]) to
/// exercise the CROSSED paths before the flip; that hook does not exist outside test builds.
#[inline]
pub fn cross_margin_enabled() -> bool {
    #[cfg(test)]
    if test_gate::enabled() {
        return true;
    }
    CROSS_MARGIN_ENABLED
}

/// The margin type IN FORCE for `pos` — the ONE implementation of the gate
/// ([`MarginType::effective`] under [`cross_margin_enabled`]). Total: no error path, so it is safe
/// under views, events and the liquidation sweep.
#[inline]
pub fn effective_margin_type(pos: &PerpPosition) -> MarginType {
    MarginType::effective(pos.margin_type, cross_margin_enabled())
}

/// `#[cfg(test)]`-only override of [`CROSS_MARGIN_ENABLED`], per THREAD (the test harness runs
/// each test on its own thread, so one test enabling it cannot leak into another).
#[cfg(test)]
pub(crate) mod test_gate {
    use core::cell::Cell;

    std::thread_local! {
        static ENABLED: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) fn enabled() -> bool {
        ENABLED.with(Cell::get)
    }

    /// Enables cross margin on this thread until the returned guard drops.
    #[must_use = "cross margin is enabled only while the guard is alive"]
    pub(crate) fn enable() -> Guard {
        ENABLED.with(|c| c.set(true));
        Guard(())
    }

    pub(crate) struct Guard(());

    impl Drop for Guard {
        fn drop(&mut self) {
            ENABLED.with(|c| c.set(false));
        }
    }
}

#[cfg(test)]
#[path = "margin_type_tests.rs"]
mod tests;
