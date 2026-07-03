//! Configuration loading and validation.
//!
//! Configuration comes from a TOML file (see `config.example.toml`) with a
//! small set of environment overrides applied on top:
//!
//! - `HIRES_LISTEN`: overrides `server.listen`
//! - `HIRES_SAMPLING_PERIOD`: overrides `sampling.period` (humantime, e.g. "200ms")
//! - `HIRES_PROCFS_ROOT`: overrides `procfs_root` (useful for testing)

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub server: ServerConfig,
    pub sampling: SamplingConfig,
    pub aggregation: AggregationConfig,
    pub collectors: CollectorsConfig,
    /// Root of the proc filesystem. Overridable for tests.
    pub procfs_root: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            sampling: SamplingConfig::default(),
            aggregation: AggregationConfig::default(),
            collectors: CollectorsConfig::default(),
            procfs_root: PathBuf::from("/proc"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    /// Address to bind the HTTP export server to.
    pub listen: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:9918".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SamplingConfig {
    /// How often collectors are polled. Sub-second periods are supported.
    #[serde(with = "humantime_serde")]
    pub period: Duration,
    /// Guardrail: the sampling period may not be configured below this.
    #[serde(with = "humantime_serde")]
    pub min_period: Duration,
    /// How often percentile snapshots are recomputed and published.
    #[serde(with = "humantime_serde")]
    pub snapshot_interval: Duration,
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            period: Duration::from_millis(200),
            min_period: Duration::from_millis(50),
            snapshot_interval: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AggregationConfig {
    /// Rolling window lengths over which percentiles are computed.
    #[serde(deserialize_with = "deserialize_duration_vec")]
    pub windows: Vec<Duration>,
    /// Quantiles to export, each in (0, 1).
    pub quantiles: Vec<f64>,
    /// Hard cap on the number of distinct series kept in windows.
    pub max_series: usize,
}

impl Default for AggregationConfig {
    fn default() -> Self {
        Self {
            windows: vec![Duration::from_secs(10), Duration::from_secs(60)],
            quantiles: vec![0.5, 0.9, 0.95, 0.99],
            max_series: 5000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CollectorsConfig {
    pub cpu: bool,
    pub disk: DiskCollectorConfig,
    pub psi: bool,
    pub run_queue: bool,
    pub softnet: bool,
    pub vmstat: bool,
}

impl Default for CollectorsConfig {
    fn default() -> Self {
        Self {
            cpu: true,
            disk: DiskCollectorConfig::default(),
            psi: true,
            run_queue: true,
            softnet: true,
            vmstat: true,
        }
    }
}

/// Disk collector settings. Accepts either a plain boolean
/// (`disk = false`) or a detailed table (`[collectors.disk]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DiskCollectorConfig {
    Flag(bool),
    Detailed(DiskCollectorOptions),
}

impl Default for DiskCollectorConfig {
    fn default() -> Self {
        Self::Detailed(DiskCollectorOptions::default())
    }
}

impl DiskCollectorConfig {
    pub fn enabled(&self) -> bool {
        match self {
            Self::Flag(enabled) => *enabled,
            Self::Detailed(opts) => opts.enabled,
        }
    }

    pub fn options(&self) -> DiskCollectorOptions {
        match self {
            Self::Flag(_) => DiskCollectorOptions::default(),
            Self::Detailed(opts) => opts.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DiskCollectorOptions {
    pub enabled: bool,
    /// If non-empty, only these device names are sampled.
    pub include: Vec<String>,
    /// Device names to skip (applied after `include`).
    pub exclude: Vec<String>,
    /// Also export a host-wide aggregate series with `device="__all__"`.
    pub aggregate_all: bool,
    /// Cap on the number of distinct devices to track.
    pub max_devices: usize,
}

impl Default for DiskCollectorOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            include: Vec::new(),
            exclude: Vec::new(),
            aggregate_all: true,
            max_devices: 64,
        }
    }
}

fn deserialize_duration_vec<'de, D>(deserializer: D) -> Result<Vec<Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<String>::deserialize(deserializer)?;
    raw.iter()
        .map(|s| humantime::parse_duration(s).map_err(serde::de::Error::custom))
        .collect()
}

impl Config {
    pub fn load(path: Option<&std::path::Path>) -> Result<Self, ConfigError> {
        let mut config: Config = match path {
            Some(path) => {
                let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
                toml::from_str(&raw)?
            }
            None => Config::default(),
        };
        config.apply_env_overrides()?;
        config.validate()?;
        Ok(config)
    }

    fn apply_env_overrides(&mut self) -> Result<(), ConfigError> {
        if let Ok(listen) = std::env::var("HIRES_LISTEN") {
            self.server.listen = listen;
        }
        if let Ok(period) = std::env::var("HIRES_SAMPLING_PERIOD") {
            self.sampling.period = humantime::parse_duration(&period).map_err(|e| {
                ConfigError::Invalid(format!("HIRES_SAMPLING_PERIOD {period:?}: {e}"))
            })?;
        }
        if let Ok(root) = std::env::var("HIRES_PROCFS_ROOT") {
            self.procfs_root = PathBuf::from(root);
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |msg: String| Err(ConfigError::Invalid(msg));

        if self.sampling.period < self.sampling.min_period {
            return invalid(format!(
                "sampling.period {:?} is below sampling.min_period {:?}",
                self.sampling.period, self.sampling.min_period
            ));
        }
        if self.sampling.period.is_zero() {
            return invalid("sampling.period must be greater than zero".into());
        }
        if self.sampling.snapshot_interval.is_zero() {
            return invalid("sampling.snapshot_interval must be greater than zero".into());
        }
        if self.aggregation.windows.is_empty() {
            return invalid("aggregation.windows must not be empty".into());
        }
        let mut seen = BTreeSet::new();
        for window in &self.aggregation.windows {
            if *window <= self.sampling.period {
                return invalid(format!(
                    "aggregation window {:?} must be longer than sampling.period {:?}",
                    window, self.sampling.period
                ));
            }
            if !seen.insert(*window) {
                return invalid(format!("duplicate aggregation window {window:?}"));
            }
        }
        if self.aggregation.quantiles.is_empty() {
            return invalid("aggregation.quantiles must not be empty".into());
        }
        for q in &self.aggregation.quantiles {
            if !(*q > 0.0 && *q < 1.0) {
                return invalid(format!("quantile {q} must be in (0, 1)"));
            }
        }
        if self.aggregation.max_series == 0 {
            return invalid("aggregation.max_series must be greater than zero".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn parses_design_example() {
        let raw = r#"
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
            psi = true
            run_queue = true
            softnet = true
            vmstat = true

            [collectors.disk]
            include = ["nvme0n1", "sda"]
            aggregate_all = true
        "#;
        let config: Config = toml::from_str(raw).unwrap();
        config.validate().unwrap();
        assert!(config.collectors.disk.enabled());
        assert_eq!(
            config.collectors.disk.options().include,
            vec!["nvme0n1", "sda"]
        );
    }

    #[test]
    fn disk_accepts_plain_bool() {
        let config: Config = toml::from_str("[collectors]\ndisk = false\n").unwrap();
        assert!(!config.collectors.disk.enabled());
    }

    #[test]
    fn rejects_bad_quantile() {
        let mut config = Config::default();
        config.aggregation.quantiles = vec![1.5];
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_window_shorter_than_period() {
        let mut config = Config::default();
        config.sampling.period = Duration::from_secs(30);
        config.sampling.min_period = Duration::from_secs(1);
        assert!(config.validate().is_err());
    }
}
