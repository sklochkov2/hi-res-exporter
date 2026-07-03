//! The high-frequency sampling loop.
//!
//! A single task owns all collectors and the aggregator (single-writer, per
//! the design's concurrency model). Every `sampling.period` it polls the
//! collectors; every `sampling.snapshot_interval` it renders a fresh
//! exposition document into the shared [`SnapshotStore`].

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use crate::aggregate::Aggregator;
use crate::collector::{CollectError, Collector, MetricDesc, SamplePoint};
use crate::config::Config;
use crate::snapshot::{SelfMetrics, SnapshotStore, render};

/// How many consecutive errors before a collector's failures stop being
/// logged at warn level (they keep counting in metrics).
const LOG_ERROR_LIMIT: u64 = 5;

struct ManagedCollector {
    collector: Box<dyn Collector>,
    enabled: bool,
    consecutive_errors: u64,
}

pub struct Sampler {
    config: Config,
    collectors: Vec<ManagedCollector>,
    descriptors: Vec<MetricDesc>,
    aggregator: Aggregator,
    self_metrics: Arc<SelfMetrics>,
    store: Arc<SnapshotStore>,
    last_tick: Option<Instant>,
}

impl Sampler {
    pub fn new(
        config: Config,
        collectors: Vec<Box<dyn Collector>>,
        store: Arc<SnapshotStore>,
    ) -> Self {
        let self_metrics = Arc::new(SelfMetrics::new(
            collectors.iter().map(|c| c.name()).collect::<Vec<_>>(),
        ));
        let descriptors: Vec<MetricDesc> = collectors
            .iter()
            .flat_map(|c| c.descriptors().iter().copied())
            .collect();
        let aggregator = Aggregator::new(&config.aggregation);
        Self {
            config,
            collectors: collectors
                .into_iter()
                .map(|collector| ManagedCollector {
                    collector,
                    enabled: true,
                    consecutive_errors: 0,
                })
                .collect(),
            descriptors,
            aggregator,
            self_metrics,
            store,
            last_tick: None,
        }
    }

    pub fn self_metrics(&self) -> Arc<SelfMetrics> {
        Arc::clone(&self.self_metrics)
    }

    /// Run one sampling pass: poll all enabled collectors and feed the
    /// aggregator. Exposed for tests.
    pub fn sample_once(&mut self, now: Instant) {
        if let Some(last) = self.last_tick {
            self.self_metrics
                .effective_sample_interval_seconds
                .set(now.duration_since(last).as_secs_f64());
        }
        self.last_tick = Some(now);

        let mut samples: Vec<SamplePoint> = Vec::with_capacity(64);
        for managed in &mut self.collectors {
            if !managed.enabled {
                continue;
            }
            let name = managed.collector.name();
            let metrics = &self.self_metrics.collectors[name];
            let before = samples.len();
            match managed.collector.collect(now, &mut samples) {
                Ok(()) => {
                    managed.consecutive_errors = 0;
                    metrics
                        .samples_total
                        .fetch_add((samples.len() - before) as u64, Ordering::Relaxed);
                }
                Err(CollectError::Unavailable(reason)) => {
                    warn!(collector = name, %reason, "disabling collector");
                    managed.enabled = false;
                    metrics.enabled.store(0, Ordering::Relaxed);
                    samples.truncate(before);
                }
                Err(error) => {
                    metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    managed.consecutive_errors += 1;
                    if managed.consecutive_errors <= LOG_ERROR_LIMIT {
                        warn!(collector = name, %error, "collect failed");
                    } else {
                        debug!(collector = name, %error, "collect failed (suppressed)");
                    }
                    samples.truncate(before);
                }
            }
        }
        self.aggregator.insert(now, samples);
    }

    /// Render and publish a snapshot of current percentiles. Exposed for tests.
    pub fn publish_snapshot(&self, now: Instant) {
        let samples = self.aggregator.snapshot(now);
        let rendered = render(
            &samples,
            &self.descriptors,
            &self.self_metrics,
            self.aggregator.stats(),
        );
        self.store.publish(rendered);
    }

    /// Drive sampling and snapshot publication until cancelled.
    pub async fn run(mut self) {
        let period = self.config.sampling.period;
        let snapshot_interval = self.config.sampling.snapshot_interval;
        info!(
            period = %humantime::format_duration(period),
            snapshot_interval = %humantime::format_duration(snapshot_interval),
            collectors = self.collectors.len(),
            "sampler started"
        );

        let mut sample_tick = tokio::time::interval(period);
        sample_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut snapshot_tick = tokio::time::interval(snapshot_interval);
        snapshot_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                scheduled = sample_tick.tick() => {
                    let started = Instant::now();
                    self.self_metrics
                        .sample_loop_lag_seconds
                        .set(started.saturating_duration_since(scheduled.into_std()).as_secs_f64());
                    self.sample_once(started);
                    self.self_metrics
                        .sample_loop_duration_seconds
                        .set(started.elapsed().as_secs_f64());
                }
                _ = snapshot_tick.tick() => {
                    self.publish_snapshot(Instant::now());
                }
            }
        }
    }
}
