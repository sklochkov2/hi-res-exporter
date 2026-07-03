# High-Resolution Linux Load Prometheus Exporter - Design

## 1. Purpose and Problem Statement

Typical host monitoring pipelines export CPU, I/O, and pressure metrics at 15-60s resolution. This smooths out short spikes that can still cause tail-latency regressions in services.

This exporter is designed to:

- sample Linux load signals at high frequency (including sub-second periods),
- retain short-window distributions of those signals,
- export Prometheus gauges representing percentiles of the sampled values,
- make brief load bursts visible to dashboards and alerts.

Primary target environment: Linux hosts with `/proc` and `/sys` available.

## 2. Goals and Non-Goals

### 2.1 Goals

- Poll system sources at configurable period; support periods `< 1s`.
- Export percentiles for:
  - CPU user load,
  - CPU system load,
  - CPU iowait load,
  - block I/O queue size.
- Include other short-spike-relevant load parameters that often correlate with response time degradation.
- Keep design modular so additional sources can be added with minimal changes.
- Keep runtime overhead low and predictable.

### 2.2 Non-Goals

- Per-process profiling or attribution (host-level exporter only in v1).
- Long-term storage or local TSDB (Prometheus scraping is the storage path).
- Complex adaptive sampling logic in v1.

## 3. High-Level Architecture

The exporter is split into six layers:

1. **Config Layer**  
   Loads validated runtime config (sampling period, windows, enabled collectors, labels).

2. **Sampler/Scheduler Layer**  
   Drives periodic sampling at high frequency and dispatches to collector plugins.

3. **Collector Layer**  
   Reads one data source (for example CPU, block devices, PSI), converts raw state to normalized samples.

4. **Aggregation Layer**  
   Maintains streaming/discrete windows and computes configured percentiles.

5. **Metric Registry Layer**  
   Maps computed results into Prometheus gauges with stable names and labels.

6. **HTTP Export Layer**  
   Exposes `/metrics`, `/healthz`, `/readyz`.

### 3.1 Data Flow

1. On each tick (`sample_period`), scheduler invokes all enabled collectors.
2. Each collector emits one or more `SamplePoint`s (`metric_key`, value, labels, timestamp).
3. Aggregator inserts samples into per-metric windows.
4. On scrape, current percentile values are read and published as gauges.
5. Exporter also publishes self-metrics (sampling lag, errors, dropped samples).

## 4. Linux Data Sources and Selected Signals

## 4.1 Core Required Signals

### CPU user/system/iowait load percentages

Source: `/proc/stat` (`cpu` aggregate line).

Approach:

- Read cumulative jiffies by mode.
- On each sample, compute deltas versus prior sample.
- `total_delta = sum(all mode deltas)`.
- Percent by mode:
  - `user_pct = 100 * (user + nice)_delta / total_delta`
  - `system_pct = 100 * (system + irq + softirq)_delta / total_delta`
  - `iowait_pct = 100 * iowait_delta / total_delta`

Why useful for bursts: captures short CPU-mode saturation windows that disappear in 1m averages.

### I/O queue size

Primary source: `/proc/diskstats` weighted queue counters.

Preferred derivation:

- Use field `weighted_time_in_io_ms` and compute:
  - `avg_queue_depth = weighted_time_delta_ms / elapsed_ms`
- Optionally also derive:
  - `util_pct = 100 * io_time_delta_ms / elapsed_ms` (device busy ratio)

Fallback:

- `/sys/block/<dev>/stat` when `/proc/diskstats` parsing is constrained.

Aggregation scope:

- per-device (label: `device`), and
- optional fleet-like host aggregate (`device="__all__"`).

## 4.2 Additional Spike-Sensitive Signals (Recommended)

These frequently correlate with transient latency spikes and should be enabled by default:

### CPU Pressure Stall Information (PSI)

Source: `/proc/pressure/cpu`.

Signals:

- `some` and `full` stall percentages over sampled interval (derived from `total` deltas).

Why:

- Captures runnable contention and scheduler delay not obvious from CPU utilization alone.

### Memory PSI

Source: `/proc/pressure/memory`.

Signals:

- `some` and `full` stall percentages (from `total` deltas).

Why:

- Brief reclaim/compaction pressure can degrade response time even if average memory use looks normal.

### I/O PSI

Source: `/proc/pressure/io`.

Signals:

- `some` and `full` stall percentages.

Why:

- Directly reflects task stalls on I/O paths beyond device-level queue depth.

### Run queue depth

Source: `/proc/loadavg` (runnable entities field) and optionally `/proc/schedstat`.

Signals:

- instant runnable count (`nr_running` style value),
- normalized runnable per CPU.

Why:

- Short runnable spikes indicate scheduling contention and tail latency risk.

### Context-switch and interrupt rates

Source: `/proc/stat` (`ctxt`, `intr` counters).

Signals:

- context switches/sec,
- interrupts/sec.

Why:

- Surges can indicate lock contention, packet storms, or interrupt pressure.

### Softnet backlog/drop pressure (network receive path)

Source: `/proc/net/softnet_stat`.

Signals:

- dropped packets delta/sec,
- time_squeezed delta/sec,
- backlog processed rates.

Why:

- Micro-bursts in network path can produce request latency spikes before long-window CPU metrics move.

### Major page fault rate

Source: `/proc/vmstat` (`pgmajfault`).

Why:

- Spikes imply disk-backed faults that can amplify latency.

## 5. Sampling and Time Semantics

## 5.1 Sampling Clock

- Use `tokio::time::interval_at` with `MissedTickBehavior::Skip`.
- Target period default: `200ms` (configurable).
- Minimum supported period: `50ms` (guardrail configurable; lower periods increase overhead and jiffy quantization effects).
- Timestamp with monotonic clock for deltas; wall clock only for exposition timestamps if needed.

## 5.2 Counter-to-Rate Conversion

Many sources are cumulative counters. For each metric instance:

- Keep previous `(value, mono_time)`.
- Compute `delta = max(0, curr - prev)` (counter reset protected).
- Convert to rate/percentage using elapsed monotonic duration.

If elapsed duration is too small or invalid, skip sample and increment exporter error counter.

## 5.3 Quantization Caveat

CPU jiffy counters have coarse resolution depending on kernel HZ. At very short periods, single-sample percentages can be noisy.

Mitigation:

- maintain percentile windows over many samples,
- expose an internal `effective_sample_interval_seconds` gauge,
- document recommended period range (`100-500ms` for most hosts).

## 6. Percentile Aggregation Design

## 6.1 Window Model

Use rolling windows over recent samples; default windows:

- `10s` (burst visibility),
- `60s` (comparison with traditional views).

Each window computes configured percentiles:

- `p50`, `p90`, `p95`, `p99` by default.

## 6.2 Aggregation Algorithm

For v1, use exact sliding buffers per metric series:

- ring buffer of `(timestamp, value)`,
- eviction by time cutoff (`now - window`),
- percentile computed by partial sort/select on scrape (or at update cadence).

Why exact first:

- bounded memory at host scale,
- straightforward correctness,
- easiest to validate during early rollout.

Upgrade path:

- optional HDRHistogram or t-digest backend for larger cardinality.
- keep this behind a trait so aggregation engine is swappable.

## 6.3 Data Model Types (Rust)

```rust
struct SamplePoint {
    key: MetricKey,
    timestamp_mono_ns: u64,
    value: f64,
}

struct MetricKey {
    family: &'static str,
    labels: SmallVec<[Label; 4]>,
}

trait Collector: Send + Sync {
    fn name(&self) -> &'static str;
    fn collect(&mut self, now: Instant, out: &mut Vec<SamplePoint>) -> Result<(), CollectError>;
}

trait PercentileWindow: Send + Sync {
    fn insert(&mut self, ts_ns: u64, value: f64);
    fn snapshot_percentiles(&self, ps: &[f64]) -> Vec<(f64, f64)>;
}
```

## 7. Prometheus Metric Schema

All exported percentile gauges follow:

`hires_<signal>_percentile{window="<duration>",quantile="<q>",...}`

Where:

- `window`: rolling window length (`10s`, `60s`),
- `quantile`: percentile (`0.5`, `0.9`, `0.95`, `0.99`),
- additional labels only when necessary (for example `device`).

### 7.1 Core Gauges

- `hires_cpu_user_percentile`
- `hires_cpu_system_percentile`
- `hires_cpu_iowait_percentile`
- `hires_io_queue_depth_percentile`

Units:

- CPU mode metrics: percent (`0..100`),
- queue depth: average concurrent requests (dimensionless count).

### 7.2 Additional Gauges

- `hires_psi_cpu_some_percentile`
- `hires_psi_cpu_full_percentile` (if kernel provides full)
- `hires_psi_memory_some_percentile`
- `hires_psi_memory_full_percentile`
- `hires_psi_io_some_percentile`
- `hires_psi_io_full_percentile`
- `hires_run_queue_depth_percentile`
- `hires_ctxt_switches_per_sec_percentile`
- `hires_interrupts_per_sec_percentile`
- `hires_softnet_drops_per_sec_percentile`
- `hires_softnet_time_squeezed_per_sec_percentile`
- `hires_pgmajfault_per_sec_percentile`

### 7.3 Exporter Self-Metrics

- `hires_exporter_collect_errors_total{collector=...}`
- `hires_exporter_samples_total{collector=...}`
- `hires_exporter_sample_loop_duration_seconds`
- `hires_exporter_sample_loop_lag_seconds`
- `hires_exporter_window_series`
- `hires_exporter_window_points`
- `hires_exporter_build_info{version,revision,rustc}`

## 8. Configuration

Support both `TOML` file and environment overrides.

Example:

```toml
[server]
listen = "0.0.0.0:9918"

[sampling]
period = "200ms"

[aggregation]
windows = ["10s", "60s"]
quantiles = [0.5, 0.9, 0.95, 0.99]
max_series = 5000

[collectors]
cpu = true
disk = true
psi = true
run_queue = true
softnet = true
vmstat = true

[collectors.disk]
include = ["nvme0n1", "sda"]
aggregate_all = true
```

Validation rules:

- period must be `>= min_period`,
- quantiles must be in `(0,1)`,
- windows unique and > period,
- cap on device cardinality to avoid unbounded memory.

## 9. Extensibility Model

Design for easy addition of new sources:

- each source implements `Collector`,
- each collector registers produced metric families in a static descriptor list,
- collector manager wires enabled collectors from config.

Adding a new source should require:

1. Implement collector file/module.
2. Register in collector factory.
3. Add metric descriptors and documentation entry.
4. Add parser/unit tests.

No aggregator or HTTP code changes should be required for most additions.

## 10. Error Handling and Robustness

- Collector failures are isolated; one bad source does not stop others.
- Errors increment per-collector counters and are rate-limited in logs.
- Missing kernel features (for example no PSI) are handled gracefully:
  - collector disables itself,
  - emits capability gauge `hires_exporter_collector_enabled{collector=...}`.
- Counter resets and hotplug events (disk devices) handled without panic.

## 11. Performance and Overhead Considerations

- Use buffered file reads and reuse allocations.
- Keep per-tick heap allocations minimal (pre-allocated sample vector).
- Parse only required files each interval.
- Device filtering to cap per-sample work.

Expected overhead target on typical host:

- CPU: <1% at 200ms period with default collectors.
- Memory: O(series * points_per_window), bounded by config limits.

## 12. Concurrency Model

Two viable options:

1. **Single-threaded sampler loop (preferred v1):**
   - deterministic timing,
   - simpler stateful counter deltas,
   - lower lock contention.

2. **Parallel collectors (future option):**
   - useful if expensive collectors are added.

V1 recommendation: one async task for sampling + one for HTTP; shared state via `Arc<RwLock<...>>` or lock-free snapshot swapping.

## 13. Security and Deployment

- Run as non-root whenever possible.
- Read-only access to `/proc` and `/sys`.
- Bind HTTP to loopback by default in examples.
- Optional TLS/auth should be left to sidecar/reverse proxy in v1.
- Provide systemd unit with hardening:
  - `NoNewPrivileges=true`
  - `ProtectSystem=strict`
  - `ProtectKernelTunables=true`
  - `ProtectControlGroups=true`
  - `PrivateTmp=true`

## 14. Testing Strategy

### 14.1 Unit Tests

- Parsers for `/proc/stat`, `/proc/diskstats`, PSI files, `/proc/net/softnet_stat`, `/proc/vmstat`.
- Delta/rate conversion edge cases (reset, wrap, zero elapsed).
- Percentile correctness for windows and quantile sets.

### 14.2 Integration Tests

- Fixture-based fake procfs directory.
- End-to-end scrape test validating metric names/labels and value ranges.

### 14.3 Performance Tests

- Benchmark sampler loop at different periods (`50ms`, `100ms`, `200ms`, `500ms`).
- Validate memory bound behavior at max configured series/window.

## 15. Suggested Rust Crates

- `tokio` - async runtime and interval scheduling.
- `prometheus-client` (or `prometheus`) - metric exposition.
- `serde`, `toml`, `figment`/`config` - configuration loading.
- `thiserror` - structured errors.
- `smallvec` - reduce small-label allocations.
- `tracing`, `tracing-subscriber` - logging/diagnostics.

Avoid heavy dependencies unless needed for percentile backend upgrades.

## 16. Rollout Plan (Phased)

1. **Phase 1 (MVP):**
   - CPU user/system/iowait percentiles,
   - disk queue depth percentiles,
   - exporter self-metrics.

2. **Phase 2:**
   - PSI CPU/memory/io percentiles,
   - run queue and context-switch/interrupt percentile metrics.

3. **Phase 3:**
   - softnet and major-fault percentile metrics,
   - optional advanced percentile backend.

4. **Phase 4:**
   - tuning docs and reference dashboards/alerts.

## 17. Open Design Decisions

- Whether to expose both host aggregate and per-CPU mode percentiles in v1.
- Whether to export raw instant sample gauges in addition to percentile gauges.
- Whether percentile computation occurs strictly on scrape or on periodic snapshot cadence.
- Whether to include histograms in addition to percentile gauges for PromQL-side quantiles.

Current recommendation:

- host aggregate only for v1,
- percentile gauges + minimal self-metrics,
- compute snapshots at fixed cadence (for example 1s) to avoid expensive per-scrape recomputation.

## 18. Example Alert Ideas

- High bursty CPU iowait:
  - `hires_cpu_iowait_percentile{window="10s",quantile="0.99"} > 20`
- I/O queue bursts:
  - `hires_io_queue_depth_percentile{window="10s",quantile="0.99",device!=""} > 8`
- CPU pressure spikes:
  - `hires_psi_cpu_some_percentile{window="10s",quantile="0.99"} > 10`

These are starting points; thresholds are workload-specific.

---

This design intentionally favors correctness, observability, and extensibility first, while leaving room for future optimizations (parallel collection and sketch-based quantiles) once real-world cardinality and performance data are available.
