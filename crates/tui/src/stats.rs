//! Frame-time bookkeeping: a bounded ring of the most recent frame render
//! durations with nearest-rank percentiles for the debug line and the
//! [`crate::RunSummary`].

use std::collections::VecDeque;

/// How many recent frame durations to keep.
const CAPACITY: usize = 240;

/// A ring of the last [`CAPACITY`] frame render durations, in milliseconds.
#[derive(Debug, Default)]
pub struct FrameTimes {
    samples: VecDeque<f32>,
}

impl FrameTimes {
    /// An empty ring.
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(CAPACITY),
        }
    }

    /// Record one frame's render time in milliseconds, evicting the oldest
    /// sample once the ring is full.
    pub fn push(&mut self, ms: f32) {
        if self.samples.len() == CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(ms);
    }

    /// The (p50, p99) render times in milliseconds over the samples held, using
    /// the nearest-rank method. `(0.0, 0.0)` while empty.
    pub fn percentiles(&self) -> (f32, f32) {
        (self.percentile(50), self.percentile(99))
    }

    /// Nearest-rank percentile `p` (`0..=100`) over the current samples.
    fn percentile(&self, p: u32) -> f32 {
        let n = self.samples.len();
        if n == 0 {
            return 0.0;
        }
        let mut sorted: Vec<f32> = self.samples.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // Nearest-rank: rank = ceil(p/100 * n), clamped to 1..=n, index = rank-1.
        let rank = ((f64::from(p) / 100.0) * n as f64).ceil() as usize;
        let idx = rank.clamp(1, n) - 1;
        sorted[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_ring_is_zero() {
        assert_eq!(FrameTimes::new().percentiles(), (0.0, 0.0));
    }

    #[test]
    fn percentiles_over_a_known_set() {
        // 1..=100 ms: nearest-rank p50 = 50, p99 = 99.
        let mut ft = FrameTimes::new();
        for i in 1..=100 {
            ft.push(i as f32);
        }
        let (p50, p99) = ft.percentiles();
        assert_eq!(p50, 50.0);
        assert_eq!(p99, 99.0);
    }

    #[test]
    fn ring_is_bounded_and_keeps_the_newest() {
        let mut ft = FrameTimes::new();
        // Push more than capacity; only the last CAPACITY survive.
        for i in 0..(CAPACITY + 50) {
            ft.push(i as f32);
        }
        assert_eq!(ft.samples.len(), CAPACITY);
        // Oldest kept is (50), newest is (289); median is well above 50.
        let (p50, _) = ft.percentiles();
        assert!(p50 > 50.0);
    }
}
