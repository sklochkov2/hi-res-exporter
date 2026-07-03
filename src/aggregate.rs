//! Rolling-window percentile aggregation.
//!
//! Each distinct series (metric family + labels) keeps one time-ordered
//! buffer covering the longest configured window; shorter windows are
//! evaluated as suffixes of that buffer. Percentiles are computed exactly
//! (sort + linear interpolation) at snapshot cadence, per the design's
//! "exact first" decision. The [`PercentileEngine`] trait keeps the backend
//! swappable (e.g. for a future t-digest implementation).

use std::collections::VecDeque;
use std::collections::hash_map::{Entry, HashMap};
use std::time::{Duration, Instant};

use crate::collector::{MetricKey, SamplePoint};
use crate::config::AggregationConfig;

/// A percentile computation backend over one series' recent samples.
pub trait PercentileEngine: Send {
    fn insert(&mut self, at: Instant, value: f64);
    /// Percentiles of samples newer than `cutoff`; `None` if empty.
    fn percentiles(&self, cutoff: Instant, quantiles: &[f64]) -> Option<Vec<f64>>;
    fn evict_before(&mut self, cutoff: Instant);
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Exact engine: ring buffer of (timestamp, value), sorted copy on demand.
#[derive(Debug, Default)]
pub struct ExactWindow {
    points: VecDeque<(Instant, f64)>,
}

impl PercentileEngine for ExactWindow {
    fn insert(&mut self, at: Instant, value: f64) {
        self.points.push_back((at, value));
    }

    fn percentiles(&self, cutoff: Instant, quantiles: &[f64]) -> Option<Vec<f64>> {
        // Points are time-ordered; binary-search the window start.
        let start = self.points.partition_point(|(t, _)| *t < cutoff);
        let mut values: Vec<f64> = self.points.iter().skip(start).map(|(_, v)| *v).collect();
        if values.is_empty() {
            return None;
        }
        values.sort_unstable_by(|a, b| a.total_cmp(b));
        Some(
            quantiles
                .iter()
                .map(|q| interpolated_quantile(&values, *q))
                .collect(),
        )
    }

    fn evict_before(&mut self, cutoff: Instant) {
        while let Some((t, _)) = self.points.front() {
            if *t < cutoff {
                self.points.pop_front();
            } else {
                break;
            }
        }
    }

    fn len(&self) -> usize {
        self.points.len()
    }
}

/// Linear-interpolation quantile of an ascending-sorted slice.
fn interpolated_quantile(sorted: &[f64], q: f64) -> f64 {
    debug_assert!(!sorted.is_empty());
    let rank = q * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = rank - lo as f64;
        sorted[lo] * (1.0 - frac) + sorted[hi] * frac
    }
}

/// One percentile result for exposition.
#[derive(Debug, Clone)]
pub struct PercentileSample {
    pub key: MetricKey,
    /// Window length label, e.g. "10s".
    pub window: String,
    /// Quantile label, e.g. "0.99".
    pub quantile: String,
    pub value: f64,
}

/// Aggregated snapshot statistics for exporter self-metrics.
#[derive(Debug, Clone, Copy, Default)]
pub struct AggregatorStats {
    pub series: usize,
    pub points: usize,
    pub dropped_series: u64,
}

pub struct Aggregator {
    windows: Vec<(Duration, String)>,
    max_window: Duration,
    quantiles: Vec<f64>,
    quantile_labels: Vec<String>,
    max_series: usize,
    series: HashMap<MetricKey, ExactWindow>,
    dropped_series: u64,
}

impl Aggregator {
    pub fn new(config: &AggregationConfig) -> Self {
        let mut windows: Vec<(Duration, String)> = config
            .windows
            .iter()
            .map(|w| (*w, humantime::format_duration(*w).to_string()))
            .collect();
        windows.sort_by_key(|(w, _)| *w);
        let max_window = windows.last().map(|(w, _)| *w).unwrap_or_default();
        Self {
            windows,
            max_window,
            quantiles: config.quantiles.clone(),
            quantile_labels: config.quantiles.iter().map(|q| q.to_string()).collect(),
            max_series: config.max_series,
            series: HashMap::new(),
            dropped_series: 0,
        }
    }

    /// Insert freshly collected samples, evicting expired points as we go.
    pub fn insert(&mut self, now: Instant, samples: Vec<SamplePoint>) {
        // `Instant` cannot go below its epoch on some platforms; guard the subtraction.
        let cutoff = now.checked_sub(self.max_window);
        for sample in samples {
            let series_count = self.series.len();
            match self.series.entry(sample.key) {
                Entry::Occupied(mut entry) => {
                    let window = entry.get_mut();
                    if let Some(cutoff) = cutoff {
                        window.evict_before(cutoff);
                    }
                    window.insert(now, sample.value);
                }
                Entry::Vacant(entry) => {
                    if series_count >= self.max_series {
                        self.dropped_series += 1;
                        continue;
                    }
                    let mut window = ExactWindow::default();
                    window.insert(now, sample.value);
                    entry.insert(window);
                }
            }
        }
    }

    /// Compute all percentile gauges as of `now`.
    pub fn snapshot(&self, now: Instant) -> Vec<PercentileSample> {
        let mut out = Vec::new();
        // Deterministic exposition order.
        let mut keys: Vec<&MetricKey> = self.series.keys().collect();
        keys.sort();
        for key in keys {
            let window = &self.series[key];
            for (duration, window_label) in &self.windows {
                let Some(cutoff) = now.checked_sub(*duration) else {
                    continue;
                };
                let Some(values) = window.percentiles(cutoff, &self.quantiles) else {
                    continue;
                };
                for (quantile_label, value) in self.quantile_labels.iter().zip(values) {
                    out.push(PercentileSample {
                        key: key.clone(),
                        window: window_label.clone(),
                        quantile: quantile_label.clone(),
                        value,
                    });
                }
            }
        }
        out
    }

    pub fn stats(&self) -> AggregatorStats {
        AggregatorStats {
            series: self.series.len(),
            points: self.series.values().map(|w| w.len()).sum(),
            dropped_series: self.dropped_series,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::MetricKey;
    use std::time::Duration;

    fn test_config() -> AggregationConfig {
        AggregationConfig {
            windows: vec![Duration::from_secs(10), Duration::from_secs(60)],
            quantiles: vec![0.5, 0.99],
            max_series: 3,
        }
    }

    fn point(family: &'static str, value: f64) -> SamplePoint {
        SamplePoint {
            key: MetricKey::plain(family),
            value,
        }
    }

    #[test]
    fn quantile_interpolation() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert!((interpolated_quantile(&values, 0.5) - 2.5).abs() < 1e-9);
        assert!((interpolated_quantile(&values, 0.0) - 1.0).abs() < 1e-9);
        assert!((interpolated_quantile(&values, 1.0) - 4.0).abs() < 1e-9);
        assert!((interpolated_quantile(&[7.0], 0.99) - 7.0).abs() < 1e-9);
    }

    #[test]
    fn windows_partition_by_time() {
        let mut agg = Aggregator::new(&test_config());
        let t0 = Instant::now();
        // Old sample outside the 10s window but inside 60s.
        agg.insert(t0, vec![point("m", 100.0)]);
        // Recent samples.
        for i in 0..5 {
            agg.insert(
                t0 + Duration::from_secs(50 + i),
                vec![point("m", 1.0 + i as f64)],
            );
        }
        let samples = agg.snapshot(t0 + Duration::from_secs(55));

        let value = |window: &str, quantile: &str| {
            samples
                .iter()
                .find(|s| s.window == window && s.quantile == quantile)
                .unwrap()
                .value
        };
        // 10s window sees only values 1..=5; p50 = 3.
        assert!((value("10s", "0.5") - 3.0).abs() < 1e-9);
        // 60s window also sees the 100.0 outlier; p99 approaches it.
        assert!(value("1m", "0.99") > 90.0);
    }

    #[test]
    fn eviction_bounds_memory() {
        let mut agg = Aggregator::new(&test_config());
        let t0 = Instant::now();
        for i in 0..1000 {
            agg.insert(
                t0 + Duration::from_millis(200 * i),
                vec![point("m", i as f64)],
            );
        }
        // Only points inside the 60s max window (300 at 200ms) survive,
        // plus tolerance for the point exactly at the cutoff.
        assert!(agg.stats().points <= 302, "points={}", agg.stats().points);
    }

    #[test]
    fn series_cap_drops_new_series() {
        let mut agg = Aggregator::new(&test_config());
        let t0 = Instant::now();
        agg.insert(
            t0,
            vec![
                point("a", 1.0),
                point("b", 1.0),
                point("c", 1.0),
                point("d", 1.0),
            ],
        );
        let stats = agg.stats();
        assert_eq!(stats.series, 3);
        assert_eq!(stats.dropped_series, 1);
    }

    #[test]
    fn empty_window_emits_nothing() {
        let agg = Aggregator::new(&test_config());
        assert!(agg.snapshot(Instant::now()).is_empty());
    }
}
