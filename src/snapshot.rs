//! Prometheus text exposition of percentile snapshots and exporter
//! self-metrics.
//!
//! The sampler renders a complete exposition string at snapshot cadence and
//! publishes it through an [`arc_swap::ArcSwap`]; scrapes just clone the
//! `Arc`, so `/metrics` never contends with the sampling loop.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use arc_swap::ArcSwap;

use crate::aggregate::{AggregatorStats, PercentileSample};
use crate::collector::MetricDesc;

/// Suffix appended to sample family names on exposition.
const PERCENTILE_SUFFIX: &str = "_percentile";

/// Lock-free holder for the latest rendered exposition.
pub struct SnapshotStore {
    rendered: ArcSwap<String>,
    ready: AtomicBool,
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self {
            rendered: ArcSwap::from_pointee(String::new()),
            ready: AtomicBool::new(false),
        }
    }

    pub fn publish(&self, rendered: String) {
        self.rendered.store(Arc::new(rendered));
        self.ready.store(true, Ordering::Release);
    }

    pub fn latest(&self) -> Arc<String> {
        self.rendered.load_full()
    }

    /// True once at least one snapshot has been published (readiness).
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Atomic f64 stored as bits; sufficient for single-writer gauges.
#[derive(Default)]
pub struct AtomicF64(AtomicU64);

impl AtomicF64 {
    pub fn set(&self, value: f64) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// Per-collector counters plus enablement state.
#[derive(Default)]
pub struct CollectorMetrics {
    pub samples_total: AtomicU64,
    pub errors_total: AtomicU64,
    /// 1 while the collector runs, 0 once disabled (e.g. missing PSI).
    pub enabled: AtomicU64,
}

/// Exporter self-metrics, updated by the sampler, rendered with every
/// snapshot.
pub struct SelfMetrics {
    /// Keyed by collector name; the collector set is fixed at startup.
    pub collectors: BTreeMap<&'static str, CollectorMetrics>,
    pub sample_loop_duration_seconds: AtomicF64,
    pub sample_loop_lag_seconds: AtomicF64,
    pub effective_sample_interval_seconds: AtomicF64,
    pub version: &'static str,
}

impl SelfMetrics {
    pub fn new(collector_names: impl IntoIterator<Item = &'static str>) -> Self {
        let collectors = collector_names
            .into_iter()
            .map(|name| {
                let metrics = CollectorMetrics::default();
                metrics.enabled.store(1, Ordering::Relaxed);
                (name, metrics)
            })
            .collect();
        Self {
            collectors,
            sample_loop_duration_seconds: AtomicF64::default(),
            sample_loop_lag_seconds: AtomicF64::default(),
            effective_sample_interval_seconds: AtomicF64::default(),
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// Escape a label value per the Prometheus text format.
fn escape_label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Render a full exposition document: percentile gauges (with HELP/TYPE from
/// collector descriptors) followed by exporter self-metrics.
pub fn render(
    samples: &[PercentileSample],
    descriptors: &[MetricDesc],
    self_metrics: &SelfMetrics,
    aggregator_stats: AggregatorStats,
) -> String {
    let help: BTreeMap<&str, &str> = descriptors.iter().map(|d| (d.family, d.help)).collect();

    let mut out = String::with_capacity(16 * 1024 + samples.len() * 96);
    let mut current_family = "";

    // `samples` arrive sorted by metric key, so families are contiguous.
    for sample in samples {
        if sample.key.family != current_family {
            current_family = sample.key.family;
            if let Some(help_text) = help.get(current_family) {
                let _ = writeln!(
                    out,
                    "# HELP {current_family}{PERCENTILE_SUFFIX} {help_text}"
                );
            }
            let _ = writeln!(out, "# TYPE {current_family}{PERCENTILE_SUFFIX} gauge");
        }
        let _ = write!(out, "{}{}{{", sample.key.family, PERCENTILE_SUFFIX);
        for label in &sample.key.labels {
            let _ = write!(
                out,
                "{}=\"{}\",",
                label.name,
                escape_label_value(&label.value)
            );
        }
        let _ = writeln!(
            out,
            "window=\"{}\",quantile=\"{}\"}} {}",
            sample.window, sample.quantile, sample.value
        );
    }

    render_self_metrics(&mut out, self_metrics, aggregator_stats);
    out
}

fn render_self_metrics(out: &mut String, m: &SelfMetrics, stats: AggregatorStats) {
    let mut counter = |name: &str, help: &str, read: &dyn Fn(&CollectorMetrics) -> u64| {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} counter");
        for (collector, metrics) in &m.collectors {
            let _ = writeln!(out, "{name}{{collector=\"{collector}\"}} {}", read(metrics));
        }
    };
    counter(
        "hires_exporter_samples_total",
        "Samples emitted per collector.",
        &|c| c.samples_total.load(Ordering::Relaxed),
    );
    counter(
        "hires_exporter_collect_errors_total",
        "Collection errors per collector.",
        &|c| c.errors_total.load(Ordering::Relaxed),
    );

    let _ = writeln!(
        out,
        "# HELP hires_exporter_collector_enabled Whether the collector is currently active."
    );
    let _ = writeln!(out, "# TYPE hires_exporter_collector_enabled gauge");
    for (collector, metrics) in &m.collectors {
        let _ = writeln!(
            out,
            "hires_exporter_collector_enabled{{collector=\"{collector}\"}} {}",
            metrics.enabled.load(Ordering::Relaxed)
        );
    }

    let mut gauge = |name: &str, help: &str, value: f64| {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        let _ = writeln!(out, "{name} {value}");
    };
    gauge(
        "hires_exporter_sample_loop_duration_seconds",
        "Time the last sampling pass took.",
        m.sample_loop_duration_seconds.get(),
    );
    gauge(
        "hires_exporter_sample_loop_lag_seconds",
        "Delay between the scheduled and actual start of the last pass.",
        m.sample_loop_lag_seconds.get(),
    );
    gauge(
        "hires_exporter_effective_sample_interval_seconds",
        "Measured interval between the last two sampling passes.",
        m.effective_sample_interval_seconds.get(),
    );
    gauge(
        "hires_exporter_window_series",
        "Distinct series currently held in percentile windows.",
        stats.series as f64,
    );
    gauge(
        "hires_exporter_window_points",
        "Total sample points currently held across all windows.",
        stats.points as f64,
    );
    gauge(
        "hires_exporter_dropped_series_total",
        "Series rejected due to the max_series cap.",
        stats.dropped_series as f64,
    );

    let _ = writeln!(out, "# HELP hires_exporter_build_info Build information.");
    let _ = writeln!(out, "# TYPE hires_exporter_build_info gauge");
    let _ = writeln!(
        out,
        "hires_exporter_build_info{{version=\"{}\"}} 1",
        m.version
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::{Label, MetricKey};

    fn sample(family: &'static str, labels: Vec<Label>) -> PercentileSample {
        PercentileSample {
            key: MetricKey::with_labels(family, labels),
            window: "10s".into(),
            quantile: "0.99".into(),
            value: 42.5,
        }
    }

    #[test]
    fn renders_percentile_gauges_with_help() {
        let descriptors = &[MetricDesc {
            family: "hires_cpu_user",
            help: "CPU user share.",
        }];
        let self_metrics = SelfMetrics::new(["cpu"]);
        let text = render(
            &[sample("hires_cpu_user", vec![])],
            descriptors,
            &self_metrics,
            AggregatorStats::default(),
        );
        assert!(text.contains("# HELP hires_cpu_user_percentile CPU user share."));
        assert!(text.contains("# TYPE hires_cpu_user_percentile gauge"));
        assert!(text.contains("hires_cpu_user_percentile{window=\"10s\",quantile=\"0.99\"} 42.5"));
        assert!(text.contains("hires_exporter_collector_enabled{collector=\"cpu\"} 1"));
        assert!(text.contains("hires_exporter_build_info{version="));
    }

    #[test]
    fn renders_device_labels_before_window() {
        let text = render(
            &[sample(
                "hires_io_queue_depth",
                vec![Label::new("device", "sda")],
            )],
            &[],
            &SelfMetrics::new([]),
            AggregatorStats::default(),
        );
        assert!(text.contains(
            "hires_io_queue_depth_percentile{device=\"sda\",window=\"10s\",quantile=\"0.99\"} 42.5"
        ));
    }

    #[test]
    fn escapes_label_values() {
        assert_eq!(escape_label_value("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn snapshot_store_readiness() {
        let store = SnapshotStore::new();
        assert!(!store.is_ready());
        store.publish("hello".into());
        assert!(store.is_ready());
        assert_eq!(*store.latest(), "hello");
    }
}
