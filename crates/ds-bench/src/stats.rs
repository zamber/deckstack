//! Latency series + percentile math for the bench (DS-033).
//!
//! The percentile convention matches the daemon's own telemetry percentile
//! (`crates/ds-daemon/src/frames/telemetry.rs::percentile_ms`): nearest-rank,
//! `idx = ceil(p * n) - 1` clamped into range, over the **sorted** samples.

/// A collected series of latency samples, in milliseconds, unsorted until queried.
#[derive(Debug, Clone, Default)]
pub struct Series {
    samples: Vec<f64>,
}

impl Series {
    pub fn new() -> Self {
        Self {
            samples: Vec::new(),
        }
    }

    /// Append one sample (ms).
    pub fn push(&mut self, ms: f64) {
        self.samples.push(ms);
    }

    /// Number of samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True when no samples were collected.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The raw samples (insertion order).
    pub fn samples(&self) -> &[f64] {
        &self.samples
    }

    /// A sorted copy of the samples.
    pub fn sorted(&self) -> Vec<f64> {
        let mut v = self.samples.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v
    }

    /// Nearest-rank percentile `p` (0..=1) of the samples; 0.0 when empty.
    pub fn percentile(&self, p: f64) -> f64 {
        percentile(&self.sorted(), p)
    }

    /// p50 / p95 / p99 in one shot (each 0.0 when empty).
    pub fn p50_p95_p99(&self) -> (f64, f64, f64) {
        let s = self.sorted();
        (
            percentile(&s, 0.50),
            percentile(&s, 0.95),
            percentile(&s, 0.99),
        )
    }

    /// The maximum sample (0.0 when empty).
    pub fn max(&self) -> f64 {
        self.samples
            .iter()
            .copied()
            .fold(0.0_f64, |a, b| if b > a { b } else { a })
    }
}

/// Nearest-rank percentile of an already-sorted slice: `idx = ceil(p * n) - 1`,
/// clamped to `[0, n-1]`; empty input yields 0.0. `p` is clamped to `[0, 1]`.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let p = p.clamp(0.0, 1.0);
    let n = sorted.len();
    let idx = ((p * n as f64).ceil() as usize).clamp(1, n) - 1;
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_series_yields_zeros() {
        let s = Series::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.percentile(0.99), 0.0);
        assert_eq!(s.p50_p95_p99(), (0.0, 0.0, 0.0));
        assert_eq!(s.max(), 0.0);
        assert_eq!(percentile(&[], 0.99), 0.0);
    }

    #[test]
    fn percentile_is_nearest_rank_like_the_daemon_telemetry() {
        // same convention as ds-daemon frames::telemetry::percentile_ms
        let sorted = [1.0, 2.0, 3.0, 4.0, 100.0];
        assert_eq!(percentile(&sorted, 0.0), 1.0);
        assert_eq!(percentile(&sorted, 0.5), 3.0); // ceil(0.5*5)=3 -> idx 2
        assert_eq!(percentile(&sorted, 0.95), 100.0); // ceil(4.75)=5 -> idx 4
        assert_eq!(percentile(&sorted, 0.99), 100.0);
        assert_eq!(percentile(&sorted, 1.0), 100.0);
        // p is clamped
        assert_eq!(percentile(&sorted, -1.0), 1.0);
        assert_eq!(percentile(&sorted, 2.0), 100.0);
    }

    #[test]
    fn percentile_is_monotonic_in_p_and_handles_single_sample() {
        let s = [7.5];
        assert_eq!(percentile(&s, 0.01), 7.5);
        assert_eq!(percentile(&s, 0.99), 7.5);
        let sorted = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        let mut prev = 0.0;
        for p in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99, 1.0] {
            let v = percentile(&sorted, p);
            assert!(v >= prev, "p={p} v={v} prev={prev}");
            prev = v;
        }
        assert_eq!(percentile(&sorted, 0.99), 10.0);
    }

    #[test]
    fn series_percentiles_over_unsorted_input_with_outliers() {
        let mut s = Series::new();
        for i in 0..100 {
            s.push(if i >= 98 {
                250.0
            } else {
                1.0 + (i % 7) as f64 * 0.1
            });
        }
        assert_eq!(s.len(), 100);
        let (p50, p95, p99) = s.p50_p95_p99();
        assert!((1.0..=1.6).contains(&p50), "p50={p50}");
        assert!(p95 < 250.0, "p95={p95} must stay under the two outliers");
        // nearest rank: ceil(0.99*100)=99 -> idx 98 -> an outlier
        assert_eq!(p99, 250.0, "p99 = the 250 ms outliers (nearest rank)");
        assert_eq!(s.max(), 250.0);
        // sorted() really sorts
        let v = s.sorted();
        assert!(v.windows(2).all(|w| w[0] <= w[1]));
    }
}
