//! Oracle price state types.

use serde::{Deserialize, Serialize};

pub const PRICE_BASIS_WINDOW_SIZE: usize = 30;

/// 30-second rolling top-of-book mid-price window for the Price 2 component.
///
/// Each sample stores a mid-price change point and its Unix-second timestamp.
/// Price2 derives basis when an oracle checkpoint closes an interval.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PriceBasisWindow {
    pub timestamps: [u64; PRICE_BASIS_WINDOW_SIZE],
    pub mid_prices: [u64; PRICE_BASIS_WINDOW_SIZE],
    pub write_idx: u8,
    pub count: u8,
    /// Unix-second timestamp of the last sample that was pushed.
    pub last_sample_ts: u64,
    /// Latest observed mid price.
    #[serde(default)]
    pub last_mid_price: u64,
}

impl Default for PriceBasisWindow {
    fn default() -> Self {
        Self {
            timestamps: [0u64; PRICE_BASIS_WINDOW_SIZE],
            mid_prices: [0u64; PRICE_BASIS_WINDOW_SIZE],
            write_idx: 0,
            count: 0,
            last_sample_ts: 0,
            last_mid_price: 0,
        }
    }
}

impl PriceBasisWindow {
    /// Pushes one mid-price sample into the ring buffer.
    pub fn push_sample(&mut self, timestamp: u64, mid_price: u64) {
        let idx = self.write_idx as usize;
        self.timestamps[idx] = timestamp;
        self.mid_prices[idx] = mid_price;
        self.write_idx = (self.write_idx + 1) % PRICE_BASIS_WINDOW_SIZE as u8;
        if (self.count as usize) < PRICE_BASIS_WINDOW_SIZE {
            self.count += 1;
        }
    }

    /// Records a top-of-book mid-price observation for `timestamp`.
    pub fn record_observation(&mut self, timestamp: u64, mid_price: u64) {
        if self.count == 0 {
            self.push_sample(timestamp, mid_price);
            self.last_sample_ts = timestamp;
            self.last_mid_price = mid_price;
            return;
        }

        if timestamp <= self.last_sample_ts {
            return;
        }

        self.push_sample(timestamp, mid_price);
        self.last_sample_ts = timestamp;
        self.last_mid_price = mid_price;
    }

    /// Time-weighted average `mid - index_at_time` over the latest basis window.
    pub fn moving_average_basis(&self, index_history: &IndexPriceHistory, end_ts: u64) -> i64 {
        if end_ts == 0 {
            return 0;
        }
        if self.count == 0 {
            return 0;
        }

        let window_start_ts = end_ts.saturating_sub(PRICE_BASIS_WINDOW_SIZE as u64);
        let mut weighted_sum = 0i128;

        let mut cursor_ts = end_ts;
        let mut mid_offset = 0usize;
        let mut active_mid = self.next_mid_before(cursor_ts, &mut mid_offset);
        let mut index_pos = index_history.checkpoints.len();
        let mut active_index = previous_index_before(index_history, cursor_ts, &mut index_pos);

        // Walk backward across every boundary where either mid or index changes.
        while cursor_ts > window_start_ts {
            let (mid_ts, mid_price) = match active_mid {
                Some(sample) => sample,
                None => break,
            };
            let (index_ts, index_price) = match active_index {
                Some(checkpoint) => checkpoint,
                None => break,
            };

            let segment_start_ts = window_start_ts.max(mid_ts).max(index_ts);
            let weight = cursor_ts.saturating_sub(segment_start_ts);
            if weight > 0 {
                weighted_sum += (mid_price as i128 - index_price as i128) * weight as i128;
            }

            cursor_ts = segment_start_ts;
            if cursor_ts == window_start_ts {
                break;
            }

            if cursor_ts == mid_ts {
                active_mid = self.next_mid_before(cursor_ts, &mut mid_offset);
            }
            if cursor_ts == index_ts {
                active_index = previous_index_before(index_history, cursor_ts, &mut index_pos);
            }
        }

        let window_weight = end_ts.saturating_sub(window_start_ts);
        if window_weight == 0 {
            0
        } else {
            (weighted_sum / window_weight as i128) as i64
        }
    }

    fn next_mid_before(&self, timestamp: u64, reverse_offset: &mut usize) -> Option<(u64, u64)> {
        let n = self.count as usize;
        while *reverse_offset < n {
            let (sample_ts, mid_price) = self.mid_sample_from_newest(*reverse_offset);
            *reverse_offset += 1;
            if sample_ts < timestamp {
                return Some((sample_ts, mid_price));
            }
        }
        None
    }

    fn mid_sample_from_newest(&self, reverse_offset: usize) -> (u64, u64) {
        let n = self.count as usize;
        let oldest_idx = if n == PRICE_BASIS_WINDOW_SIZE {
            self.write_idx as usize
        } else {
            0
        };
        let chronological_offset = n - 1 - reverse_offset;
        let idx = (oldest_idx + chronological_offset) % PRICE_BASIS_WINDOW_SIZE;
        (self.timestamps[idx], self.mid_prices[idx])
    }
}

fn previous_index_before(
    index_history: &IndexPriceHistory,
    timestamp: u64,
    index_pos: &mut usize,
) -> Option<(u64, u64)> {
    while *index_pos > 0 {
        *index_pos -= 1;
        let state = &index_history.checkpoints[*index_pos];
        if state.timestamp < timestamp {
            return Some((state.timestamp, state.index_price));
        }
    }
    None
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct IndexPriceHistory {
    pub checkpoints: Vec<IndexPriceState>,
}

impl IndexPriceHistory {
    pub fn push(&mut self, state: IndexPriceState, max_checkpoints: usize) {
        if state.index_price == 0 {
            return;
        }
        if let Some(existing) = self
            .checkpoints
            .iter_mut()
            .find(|checkpoint| checkpoint.timestamp == state.timestamp)
        {
            existing.index_price = state.index_price;
        } else {
            self.checkpoints.push(state);
        }
        self.checkpoints
            .sort_unstable_by_key(|state| state.timestamp);
        let keep = max_checkpoints.max(1);
        if self.checkpoints.len() > keep {
            let drain_len = self.checkpoints.len() - keep;
            self.checkpoints.drain(0..drain_len);
        }
    }

    pub fn price_at_or_before(&self, timestamp: u64) -> Option<u64> {
        self.checkpoints
            .iter()
            .filter(|state| state.timestamp <= timestamp)
            .max_by_key(|state| state.timestamp)
            .map(|state| state.index_price)
    }
}

/// Per-market funding state — auto-updated by updateIndexPrice at epoch boundaries.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct FundingState {
    /// Last computed funding rate in FUNDING_RATE_ONE units (1e6 = 100%).
    pub last_funding_rate: i64,
    /// Unix-second timestamp of the next funding settlement (0 = not yet started).
    pub next_funding_ts: u64,
}

/// Per-market linearly-weighted premium index accumulator for funding rate calculation.
///
/// Funding rate = weighted average of premium index over the epoch, where the k-th
/// sample (1-indexed from epoch start) has weight k.  Older samples get lower weight.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct PremiumIndexAccumulator {
    /// Σ(k × PI_k) in FUNDING_RATE_ONE = 1_000_000 units.
    pub weighted_sum: i64,
    /// Number of samples pushed in the current epoch.
    pub sample_count: u64,
    /// Unix-second timestamp of the first sample in this epoch (0 = not started).
    pub epoch_start_ts: u64,
}

impl PremiumIndexAccumulator {
    /// Push one premium-index sample.  Weight = sample_count + 1 (1-indexed).
    pub fn push_sample(&mut self, pi: i64) {
        let weight = (self.sample_count + 1) as i64;
        self.weighted_sum = self.weighted_sum.saturating_add(pi.saturating_mul(weight));
        self.sample_count += 1;
    }

    /// Linearly-weighted average: Σ(k·PI_k) / Σk = weighted_sum / (n·(n+1)/2).
    pub fn average(&self) -> i64 {
        if self.sample_count == 0 {
            return 0;
        }
        let total_weight = (self.sample_count * (self.sample_count + 1)) / 2;
        self.weighted_sum / total_weight as i64
    }
}

/// Per-market index price state written by the oracle.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct IndexPriceState {
    pub index_price: u64,
    pub timestamp: u64,
}

