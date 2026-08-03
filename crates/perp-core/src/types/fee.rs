use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct UserFeeRates {
    #[serde(rename = "MF")]
    pub maker_fee_bps: u64,

    #[serde(rename = "TF")]
    pub taker_fee_bps: u64,
}
