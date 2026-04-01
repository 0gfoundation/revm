//! User account type stored in the PerpDEX precompile's own storage.
use serde::{Deserialize, Serialize};

use crate::as_bin::AsBinStr;

/// On-chain record for a single user's DEX account.
///
/// Serialised via MessagePack (struct-map format) so that fields can be
/// added in future upgrades without breaking existing storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserAccount {
    /// USDC balance held inside the DEX (internal accounting unit).
    /// Stored as a decimal ASCII string so it survives msgpack round-trips
    /// without loss — identical to the pattern used in `wa0gi_base`.
    #[serde(rename = "UB")]
    pub usdc_balance: AsBinStr,
}

impl Default for UserAccount {
    fn default() -> Self {
        Self {
            usdc_balance: "0".into(),
        }
    }
}
