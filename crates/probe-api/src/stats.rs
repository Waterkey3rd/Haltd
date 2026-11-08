use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const BUCKETS: usize = 32;

/// Latency histogram with log2 buckets over microseconds. Recording is cheap
/// enough to run on every probe call.
#[derive(Clone, Debug)]
pub struct Histogram {
    buckets: [u64; BUCKETS],
    count: u64,
    sum_us: u64,
    max_us: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: [0; BUCKETS],
            count: 0,
            sum_us: 0,
            max_us: 0,
        }
    }
}

impl Histogram {
    pub fn record(&mut self, d: Duration) {
        let us = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        let bucket = ((u64::BITS - us.leading_zeros()) as usize).min(BUCKETS - 1);
        self.buckets[bucket] += 1;
        self.count += 1;
        self.sum_us = self.sum_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Upper bound of the bucket containing quantile `q` (0.0..=1.0), never
    /// larger than the observed maximum.
    pub fn quantile_us(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let rank = ((self.count as f64 * q).ceil() as u64).clamp(1, self.count);
        let mut seen = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                let upper = if i == 0 { 0 } else { (1u64 << i) - 1 };
                return upper.min(self.max_us);
            }
        }
        self.max_us
    }

    pub fn summary(&self) -> HistogramSummary {
        HistogramSummary {
            count: self.count,
            mean_us: self.sum_us.checked_div(self.count).unwrap_or(0),
            p50_us: self.quantile_us(0.50),
            p99_us: self.quantile_us(0.99),
            max_us: self.max_us,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistogramSummary {
    pub count: u64,
    pub mean_us: u64,
    pub p50_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

/// Per-function latency statistics, keyed by library function name.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CallStats {
    pub calls: BTreeMap<String, HistogramSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_report_bucket_upper_bounds() {
        let mut h = Histogram::default();
        for _ in 0..99 {
            h.record(Duration::from_micros(1000)); // bucket upper bound 1023
        }
        h.record(Duration::from_micros(40_000));
        let s = h.summary();
        assert_eq!(s.count, 100);
        assert_eq!(s.p50_us, 1023);
        assert_eq!(s.p99_us, 1023);
        assert_eq!(s.max_us, 40_000);
        assert_eq!(h.quantile_us(1.0), 40_000);
    }

    #[test]
    fn empty_histogram_is_all_zero() {
        assert_eq!(Histogram::default().summary(), HistogramSummary::default());
    }
}
