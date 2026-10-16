use serde::{Deserialize, Serialize};

/// A user's trading-fee rates, in ppm of notional (`math::FEE_PPM_DENOMINATOR`).
///
/// The MAKER rate is signed: a negative rate is a maker rebate. The TAKER rate is not — taker
/// rebates are not supported.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct UserFeeRates {
    #[serde(rename = "MF")]
    pub maker_fee_ppm: i64,

    #[serde(rename = "TF")]
    pub taker_fee_ppm: u64,
}
