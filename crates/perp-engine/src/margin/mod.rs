//! Margin: the margin type in force (`margin_type` — Binance's `marginType`, CROSSED / ISOLATED,
//! and its gate) and the derived margin layer (`view` — every open-order margin number, the
//! admission basis, the cross fold and health check, and the margin read surfaces).

pub mod margin_type;
pub mod view;
#[cfg(test)]
mod tests;
