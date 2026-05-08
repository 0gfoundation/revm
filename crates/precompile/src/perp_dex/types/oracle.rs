//! Oracle price state types.

use serde::{Deserialize, Serialize};

pub const PRICE_BASIS_WINDOW_SIZE: usize = 30;

/// 30-second rolling basis window for the Price 2 component of mark price.
///
/// Samples are `(best_bid + best_ask) / 2 − index_price` taken every second.
/// The ring buffer overwrites the oldest entry once all 30 slots are filled.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PriceBasisWindow {
    pub samples: [i64; PRICE_BASIS_WINDOW_SIZE],
    pub write_idx: u8,
    pub count: u8,
    /// Unix-second timestamp of the last sample that was pushed.
    pub last_sample_ts: u64,
}

impl Default for PriceBasisWindow {
    fn default() -> Self {
        Self {
            samples: [0i64; PRICE_BASIS_WINDOW_SIZE],
            write_idx: 0,
            count: 0,
            last_sample_ts: 0,
        }
    }
}

impl PriceBasisWindow {
    pub fn push_sample(&mut self, basis: i64) {
        self.samples[self.write_idx as usize] = basis;
        self.write_idx = (self.write_idx + 1) % PRICE_BASIS_WINDOW_SIZE as u8;
        if (self.count as usize) < PRICE_BASIS_WINDOW_SIZE {
            self.count += 1;
        }
    }

    /// Average over all valid samples (order-independent; uses i128 to avoid overflow).
    pub fn moving_average(&self) -> i64 {
        let n = self.count as usize;
        if n == 0 {
            return 0;
        }
        // samples[0..n] covers all valid slots regardless of ring-wrap state:
        // when n < 30 the buffer hasn't wrapped yet; when n == 30 all slots hold data.
        let sum: i128 = self.samples[..n].iter().map(|&s| s as i128).sum();
        (sum / n as i128) as i64
    }
}

/// Per-market funding configuration, set by admin after each settlement epoch.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct FundingState {
    /// Last settled funding rate in FUNDING_RATE_ONE units (1e9 = 100%).
    pub last_funding_rate: i64,
    /// Seconds between funding epochs (e.g. 28 800 for 8 h).
    pub funding_interval: u64,
    /// Unix-second timestamp of the next funding settlement.
    pub next_funding_ts: u64,
}

/// Per-market index price state written by the oracle.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct IndexPriceState {
    pub index_price: u64,
    pub timestamp: u64,
}
