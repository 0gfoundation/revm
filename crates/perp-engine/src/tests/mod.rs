//! Whole-engine suites, not owned by one subject module: the cross-margin P2 spec (accounting,
//! settlement, gates, liquidation and ADL against a model), the cross golden commitment pin, and
//! the perpdex-perf fixture exporter.

/// The independent cross-margin P2 spec suite (`SPEC_cross_margin.md` P2 ground rules, report §3.2
/// model): every surface against a model, every gate to the unit, cross liquidation and ADL.
#[cfg(test)]
mod cross_accounting_spec;

/// The cross golden scenario (cross P3, R8): the commitment pin and business snapshot of cross
/// margin as it ships, beside the isolated `trading::tests::golden`.
#[cfg(test)]
mod cross_golden;

/// Real-engine event fixtures for the perpdex-perf harness: scenarios driven through the call
/// entry point on an `InMemoryHost`, recorded as receipts plus getter reads (`#[ignore]`d tool).
#[cfg(test)]
mod fixture_export;
