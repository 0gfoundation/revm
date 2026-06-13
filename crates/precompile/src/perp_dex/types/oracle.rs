//! Oracle price state types.

use serde::{Deserialize, Serialize};

use crate::{perp_dex::errors::perp_err, PrecompileError};

pub const PRICE_BASIS_WINDOW_SIZE: usize = 30;

/// 30-second rolling top-of-book mid-price window for the Price 2 component.
///
/// Each sample stores a mid-price change point and its Unix-second timestamp.
/// Price2 derives basis when an oracle checkpoint closes an interval.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PriceBasisWindow {
    #[serde(default)]
    pub timestamps: [u64; PRICE_BASIS_WINDOW_SIZE],
    #[serde(default)]
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

    /// Records a top-of-book mid-price observation for `timestamp`. Returns `true` if the window
    /// was mutated, `false` if the observation was ignored (a same-or-older timestamp, which is
    /// every quote change after the first within a block, since the block timestamp is constant) —
    /// so the caller can skip re-storing an unchanged ~hundreds-of-bytes blob (P4/#17).
    #[must_use]
    pub fn record_observation(&mut self, timestamp: u64, mid_price: u64) -> bool {
        if self.count == 0 {
            self.push_sample(timestamp, mid_price);
            self.last_sample_ts = timestamp;
            self.last_mid_price = mid_price;
            return true;
        }

        if timestamp <= self.last_sample_ts {
            return false;
        }

        self.push_sample(timestamp, mid_price);
        self.last_sample_ts = timestamp;
        self.last_mid_price = mid_price;
        true
    }

    /// Time-weighted average `mid - index_at_time` over the latest basis window.
    pub fn moving_average_basis(
        &self,
        index_history: &IndexPriceHistory,
        end_ts: u64,
    ) -> Result<i64, PrecompileError> {
        if end_ts == 0 {
            return Ok(0);
        }
        if self.count == 0 {
            return Ok(0);
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
            Ok(0)
        } else {
            i64::try_from(weighted_sum / window_weight as i128)
                .map_err(|_| perp_err("price basis window: moving average exceeds i64 range"))
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
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct FundingState {
    /// Last computed funding rate in FUNDING_RATE_ONE units (1e6 = 100%).
    pub last_funding_rate: i64,
    /// Unix-second timestamp of the next funding settlement (0 = not yet started).
    pub next_funding_ts: u64,
    /// Cumulative funding index: Σ over epoch boundaries of `mark_price * rate`.
    /// Positions settle funding lazily against `index − position.last_funding_index`
    /// (see `calc_funding_payment`). Starts at 0 at genesis.
    #[serde(default)]
    pub cumulative_funding_index: i128,
}

/// Per-market linearly-weighted premium index accumulator for funding rate calculation.
///
/// Funding rate = weighted average of premium index over the epoch, where the k-th
/// sample (1-indexed from epoch start) has weight k.  Older samples get lower weight.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct PremiumIndexAccumulator {
    /// Σ(k × PI_k) in FUNDING_RATE_ONE = 1_000_000 units.
    pub weighted_sum: i128,
    /// Number of samples pushed in the current epoch.
    pub sample_count: u64,
    /// Unix-second timestamp of the first sample in this epoch (0 = not started).
    pub epoch_start_ts: u64,
    /// Last known premium index used for forward-filling missing sample slots.
    #[serde(default)]
    pub last_pi: i64,
    /// Timestamp of the latest filled sample slot.
    #[serde(default)]
    pub last_sample_ts: u64,
}

impl PremiumIndexAccumulator {
    /// Push one premium-index slot. Weight = sample_count + 1 (1-indexed).
    fn push_slot(&mut self, pi: i64) -> Result<(), PrecompileError> {
        let next_count = self
            .sample_count
            .checked_add(1)
            .ok_or_else(|| perp_err("premium accumulator: sample count overflow"))?;
        let weighted = (pi as i128)
            .checked_mul(next_count as i128)
            .ok_or_else(|| perp_err("premium accumulator: weighted sample overflow"))?;
        self.weighted_sum = self
            .weighted_sum
            .checked_add(weighted)
            .ok_or_else(|| perp_err("premium accumulator: weighted sum overflow"))?;
        self.sample_count = next_count;
        Ok(())
    }

    /// Starts a fresh epoch with the observed premium index at `timestamp`.
    pub fn start_epoch(
        &mut self,
        epoch_start_ts: u64,
        timestamp: u64,
        pi: i64,
    ) -> Result<(), PrecompileError> {
        self.weighted_sum = 0;
        self.sample_count = 0;
        self.epoch_start_ts = epoch_start_ts;
        self.last_pi = pi;
        self.last_sample_ts = timestamp;
        self.push_slot(pi)
    }

    /// Fills sample slots through `end_ts`.
    ///
    /// Slots before `end_ts` use the latest known PI. If `endpoint_pi` is set
    /// and `end_ts` lands exactly on a sample slot, the endpoint slot uses it.
    pub fn fill_slots_until(
        &mut self,
        end_ts: u64,
        sample_interval: u64,
        endpoint_pi: Option<i64>,
    ) -> Result<(), PrecompileError> {
        if sample_interval == 0 || self.last_sample_ts == 0 || end_ts <= self.last_sample_ts {
            return Ok(());
        }
        let mut slot_ts = self
            .last_sample_ts
            .checked_add(sample_interval)
            .ok_or_else(|| perp_err("premium accumulator: sample timestamp overflow"))?;
        while slot_ts <= end_ts {
            let pi = if endpoint_pi.is_some() && slot_ts == end_ts {
                endpoint_pi.unwrap_or(self.last_pi)
            } else {
                self.last_pi
            };
            self.push_slot(pi)?;
            self.last_pi = pi;
            self.last_sample_ts = slot_ts;
            slot_ts = match slot_ts.checked_add(sample_interval) {
                Some(next) => next,
                None => break,
            };
        }
        Ok(())
    }

    /// Linearly-weighted average: Σ(k·PI_k) / Σk = weighted_sum / (n·(n+1)/2).
    pub fn average(&self) -> Result<i64, PrecompileError> {
        if self.sample_count == 0 {
            return Ok(0);
        }
        let total_weight = (self.sample_count as u128)
            .checked_mul(self.sample_count.saturating_add(1) as u128)
            .ok_or_else(|| perp_err("premium accumulator: total weight overflow"))?
            / 2;
        let total_weight = i64::try_from(total_weight)
            .map_err(|_| perp_err("premium accumulator: total weight exceeds i64::MAX"))?;
        i64::try_from(self.weighted_sum / total_weight as i128)
            .map_err(|_| perp_err("premium accumulator: average exceeds i64 range"))
    }
}

/// Per-market index price state written by the oracle.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct IndexPriceState {
    pub index_price: u64,
    pub timestamp: u64,
}
