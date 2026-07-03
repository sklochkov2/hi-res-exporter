//! Collector plugin framework.
//!
//! Each data source implements [`Collector`]. Adding a new source requires:
//!
//! 1. a new module implementing [`Collector`] (including its
//!    [`MetricDesc`] descriptors),
//! 2. one entry in [`build_collectors`],
//! 3. parser unit tests.
//!
//! No aggregator, snapshot, or HTTP code changes are needed: sample family
//! names flow from descriptors through the aggregator into the exposition
//! automatically.

pub mod cpu;
pub mod disk;
pub mod psi;
pub mod run_queue;
pub mod softnet;
pub mod vmstat;

use std::time::Instant;

use crate::config::Config;

/// A single metric label pair. Label names are static by design: they are
/// part of a collector's schema, not runtime data.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Label {
    pub name: &'static str,
    pub value: String,
}

impl Label {
    pub fn new(name: &'static str, value: impl Into<String>) -> Self {
        Self {
            name,
            value: value.into(),
        }
    }
}

/// Identity of a sampled series: metric family plus labels.
///
/// The `family` is the base name (for example `hires_cpu_user`); the
/// exposition layer appends `_percentile` and the `window`/`quantile` labels.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MetricKey {
    pub family: &'static str,
    pub labels: Vec<Label>,
}

impl MetricKey {
    pub fn plain(family: &'static str) -> Self {
        Self {
            family,
            labels: Vec::new(),
        }
    }

    pub fn with_labels(family: &'static str, labels: Vec<Label>) -> Self {
        Self { family, labels }
    }
}

/// One high-frequency observation emitted by a collector.
#[derive(Debug, Clone)]
pub struct SamplePoint {
    pub key: MetricKey,
    pub value: f64,
}

/// Static description of a metric family produced by a collector, used to
/// emit `# HELP` / `# TYPE` exposition comments.
#[derive(Debug, Clone, Copy)]
pub struct MetricDesc {
    /// Base family name; `_percentile` is appended on exposition.
    pub family: &'static str,
    pub help: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum CollectError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(String),
    /// The source is permanently unavailable on this system (for example a
    /// kernel without PSI support). The collector will be disabled.
    #[error("source unavailable: {0}")]
    Unavailable(String),
}

/// A pollable data source.
///
/// Collectors are stateful (they typically track previous counter values to
/// derive rates) and are only ever called from the single sampler task, so
/// they need `Send` but not `Sync`.
pub trait Collector: Send {
    fn name(&self) -> &'static str;

    /// Metric families this collector can emit.
    fn descriptors(&self) -> &'static [MetricDesc];

    /// Poll the source once and append samples to `out`.
    ///
    /// The first call after startup typically emits nothing for rate-derived
    /// metrics (no previous counter values yet).
    fn collect(&mut self, now: Instant, out: &mut Vec<SamplePoint>) -> Result<(), CollectError>;
}

/// Construct all collectors enabled by the configuration.
///
/// This is the single registration point for new sources.
pub fn build_collectors(config: &Config) -> Vec<Box<dyn Collector>> {
    let root = &config.procfs_root;
    let mut collectors: Vec<Box<dyn Collector>> = Vec::new();
    if config.collectors.cpu {
        collectors.push(Box::new(cpu::CpuCollector::new(root)));
    }
    if config.collectors.disk.enabled() {
        collectors.push(Box::new(disk::DiskCollector::new(
            root,
            config.collectors.disk.options(),
        )));
    }
    if config.collectors.psi {
        collectors.push(Box::new(psi::PsiCollector::new(root)));
    }
    if config.collectors.run_queue {
        collectors.push(Box::new(run_queue::RunQueueCollector::new(root)));
    }
    if config.collectors.softnet {
        collectors.push(Box::new(softnet::SoftnetCollector::new(root)));
    }
    if config.collectors.vmstat {
        collectors.push(Box::new(vmstat::VmstatCollector::new(root)));
    }
    collectors
}

/// Tracks a cumulative counter and converts it to a per-second rate between
/// consecutive observations. Protects against counter resets and bogus
/// (zero or negative) elapsed time.
#[derive(Debug, Default, Clone)]
pub struct RateTracker {
    prev: Option<(u64, Instant)>,
}

impl RateTracker {
    /// Feed the current counter value; returns the rate per second since the
    /// previous observation, or `None` on the first call, after a counter
    /// reset, or if no time has elapsed.
    pub fn update(&mut self, value: u64, now: Instant) -> Option<f64> {
        let result = match self.prev {
            Some((prev_value, prev_time)) => {
                let elapsed = now.duration_since(prev_time).as_secs_f64();
                if elapsed <= 0.0 || value < prev_value {
                    None
                } else {
                    Some((value - prev_value) as f64 / elapsed)
                }
            }
            None => None,
        };
        self.prev = Some((value, now));
        result
    }

    /// Like [`RateTracker::update`], but returns `(delta, elapsed_seconds)`
    /// for callers that need a custom normalization (for example jiffy
    /// ratios).
    pub fn update_delta(&mut self, value: u64, now: Instant) -> Option<(u64, f64)> {
        let result = match self.prev {
            Some((prev_value, prev_time)) => {
                let elapsed = now.duration_since(prev_time).as_secs_f64();
                if elapsed <= 0.0 || value < prev_value {
                    None
                } else {
                    Some((value - prev_value, elapsed))
                }
            }
            None => None,
        };
        self.prev = Some((value, now));
        result
    }
}

pub(crate) fn parse_field<T: std::str::FromStr>(
    raw: &str,
    context: &str,
) -> Result<T, CollectError> {
    raw.parse()
        .map_err(|_| CollectError::Parse(format!("bad value {raw:?} in {context}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn rate_tracker_first_sample_yields_nothing() {
        let mut tracker = RateTracker::default();
        assert!(tracker.update(100, Instant::now()).is_none());
    }

    #[test]
    fn rate_tracker_computes_per_second_rate() {
        let mut tracker = RateTracker::default();
        let t0 = Instant::now();
        tracker.update(100, t0);
        let rate = tracker
            .update(150, t0 + Duration::from_millis(500))
            .unwrap();
        assert!((rate - 100.0).abs() < 1e-9);
    }

    #[test]
    fn rate_tracker_handles_counter_reset() {
        let mut tracker = RateTracker::default();
        let t0 = Instant::now();
        tracker.update(100, t0);
        assert!(tracker.update(10, t0 + Duration::from_secs(1)).is_none());
        // After the reset the tracker resumes from the new baseline.
        let rate = tracker.update(20, t0 + Duration::from_secs(2)).unwrap();
        assert!((rate - 10.0).abs() < 1e-9);
    }

    #[test]
    fn rate_tracker_rejects_zero_elapsed() {
        let mut tracker = RateTracker::default();
        let t0 = Instant::now();
        tracker.update(100, t0);
        assert!(tracker.update(200, t0).is_none());
    }
}
