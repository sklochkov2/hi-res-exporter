# hi-res-exporter

A Prometheus exporter that samples Linux load signals at high frequency
(sub-second) and exports **percentiles over short rolling windows**, so brief
load bursts that degrade response times stay visible even when conventional
30-60s-averaged metrics look flat.

See [DESIGN.md](DESIGN.md) for the full design document.

## What it measures

| Signal | Source | Metric families |
| --- | --- | --- |
| CPU user / system / iowait share | `/proc/stat` | `hires_cpu_{user,system,iowait}_percentile` |
| Context switch / interrupt rates | `/proc/stat` | `hires_{ctxt_switches,interrupts}_per_sec_percentile` |
| I/O queue depth and utilization | `/proc/diskstats` | `hires_io_queue_depth_percentile{device=...}`, `hires_io_util_percent_percentile{device=...}` |
| Pressure stall (CPU, memory, I/O) | `/proc/pressure/*` | `hires_psi_{cpu,memory,io}_{some,full}_percentile` |
| Runnable task count | `/proc/loadavg` | `hires_run_queue_{depth,per_cpu}_percentile` |
| Network RX backlog pressure | `/proc/net/softnet_stat` | `hires_softnet_{processed,drops,time_squeezed}_per_sec_percentile` |
| Major page fault rate | `/proc/vmstat` | `hires_pgmajfault_per_sec_percentile` |

Every percentile gauge carries `window` (e.g. `10s`, `1m`) and `quantile`
(e.g. `0.99`) labels. The exporter also publishes self-metrics
(`hires_exporter_*`: per-collector sample/error counters, loop timing,
window sizes, build info).

## Quick start

```bash
cargo build --release
./target/release/hi-res-exporter --config config.example.toml
curl -s http://127.0.0.1:9918/metrics | grep hires_cpu_iowait
```

Endpoints: `/metrics`, `/healthz`, `/readyz` (503 until the first snapshot).

## Configuration

TOML file (see [config.example.toml](config.example.toml)) plus environment
overrides `HIRES_LISTEN`, `HIRES_SAMPLING_PERIOD`, and `HIRES_PROCFS_ROOT`.
Defaults: 200ms sampling, 10s and 60s windows, quantiles 0.5/0.9/0.95/0.99.

Example burst-detection alert:

```promql
hires_cpu_iowait_percentile{window="10s",quantile="0.99"} > 20
```

## How it works

A single sampler task polls all enabled collectors every `sampling.period`
and feeds exact rolling windows (one ring buffer per series). At
`snapshot_interval` cadence it renders the full Prometheus exposition into a
lock-free slot that HTTP scrapes read without touching the sampling path.
Counters are delta-tracked with counter-reset protection; collectors that
fail permanently (for example PSI on kernels without `CONFIG_PSI`) disable
themselves and report `hires_exporter_collector_enabled 0` while the rest
keep running.

## Adding a new data source

1. Create `src/collector/<name>.rs` implementing the `Collector` trait
   (`name()`, `descriptors()`, `collect()`); see `vmstat.rs` for the
   smallest example.
2. Register it in `build_collectors` in `src/collector/mod.rs` and add a
   toggle to `CollectorsConfig` in `src/config.rs`.
3. Add parser unit tests alongside the collector.

The aggregation, exposition, and HTTP layers pick up new metric families
automatically.

## Deployment

A hardened systemd unit is provided in
[hi-res-exporter.service](hi-res-exporter.service). The exporter only needs
read access to `/proc` and runs fine as an unprivileged dynamic user.

## Development

```bash
cargo test           # unit + end-to-end tests (fake procfs fixtures)
cargo clippy --all-targets
```
