//! The margin suites: the margin-mode switch and tiers (P0/P1 spec), the cross accounting core
//! (P2a), and the account's cross legs and health check (P4, implementation and spec).

/// The independent cross-margin P0/P1 spec suite (`SPEC_cross_margin.md`).
#[cfg(test)]
mod margin_mode_spec;

/// Cross margin P2a: the accounting core (funding mirror, account fields, the cross fold) and its
/// read surfaces.
#[cfg(test)]
mod cross_accounting;

/// Cross margin P4: the account's cross-leg summary (`types::CrossLegs`) on every route, and the
/// health check priced from it (`margin_view::cross_health`).
#[cfg(test)]
mod cross_legs;

/// Cross margin P4: an independent spec suite — the cross legs and the health check recomputed
/// from the position records after every call, on every route, and at random.
#[cfg(test)]
mod cross_legs_spec;
