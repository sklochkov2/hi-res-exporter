//! High-resolution Linux load Prometheus exporter.
//!
//! See `DESIGN.md` for the full architecture. The crate is split into:
//!
//! - [`config`]: TOML + env configuration with validation,
//! - [`collector`]: the `Collector` trait and the built-in procfs collectors,
//! - [`aggregate`]: rolling-window percentile aggregation,
//! - [`snapshot`]: Prometheus text exposition of percentile snapshots,
//! - [`sampler`]: the high-frequency sampling loop,
//! - [`server`]: the HTTP export layer (`/metrics`, `/healthz`, `/readyz`).

pub mod aggregate;
pub mod collector;
pub mod config;
pub mod sampler;
pub mod server;
pub mod snapshot;
