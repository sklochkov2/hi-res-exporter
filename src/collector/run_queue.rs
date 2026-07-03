//! Run queue collector: instantaneous runnable task count from
//! `/proc/loadavg` (the `R/T` field), raw and normalized per CPU.

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{CollectError, Collector, MetricDesc, MetricKey, SamplePoint, parse_field};

pub const DESCRIPTORS: &[MetricDesc] = &[
    MetricDesc {
        family: "hires_run_queue_depth",
        help: "Instantaneous count of runnable tasks.",
    },
    MetricDesc {
        family: "hires_run_queue_per_cpu",
        help: "Instantaneous runnable tasks divided by online CPU count.",
    },
];

/// Parse the runnable count out of `/proc/loadavg`
/// (`0.42 0.36 0.30 2/1234 5678` => 2).
pub fn parse_loadavg_runnable(raw: &str) -> Result<u64, CollectError> {
    let field = raw
        .split_ascii_whitespace()
        .nth(3)
        .ok_or_else(|| CollectError::Parse("short /proc/loadavg".into()))?;
    let runnable = field
        .split('/')
        .next()
        .ok_or_else(|| CollectError::Parse(format!("bad runnable field {field:?}")))?;
    parse_field(runnable, "/proc/loadavg runnable count")
}

/// Count online CPUs from the `cpuN` lines of `/proc/stat`.
pub fn count_cpus(proc_stat: &str) -> usize {
    proc_stat
        .lines()
        .filter(|line| {
            line.strip_prefix("cpu")
                .is_some_and(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_digit()))
        })
        .count()
}

pub struct RunQueueCollector {
    loadavg_path: PathBuf,
    stat_path: PathBuf,
    num_cpus: Option<usize>,
}

impl RunQueueCollector {
    pub fn new(procfs_root: &Path) -> Self {
        Self {
            loadavg_path: procfs_root.join("loadavg"),
            stat_path: procfs_root.join("stat"),
            num_cpus: None,
        }
    }
}

impl Collector for RunQueueCollector {
    fn name(&self) -> &'static str {
        "run_queue"
    }

    fn descriptors(&self) -> &'static [MetricDesc] {
        DESCRIPTORS
    }

    fn collect(&mut self, _now: Instant, out: &mut Vec<SamplePoint>) -> Result<(), CollectError> {
        let num_cpus = match self.num_cpus {
            Some(n) => n,
            None => {
                let stat = std::fs::read_to_string(&self.stat_path)?;
                let n = count_cpus(&stat).max(1);
                self.num_cpus = Some(n);
                n
            }
        };

        let raw = std::fs::read_to_string(&self.loadavg_path)?;
        // Subtract this exporter process itself from the runnable count.
        let runnable = parse_loadavg_runnable(&raw)?.saturating_sub(1) as f64;

        out.push(SamplePoint {
            key: MetricKey::plain("hires_run_queue_depth"),
            value: runnable,
        });
        out.push(SamplePoint {
            key: MetricKey::plain("hires_run_queue_per_cpu"),
            value: runnable / num_cpus as f64,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loadavg() {
        assert_eq!(
            parse_loadavg_runnable("0.42 0.36 0.30 3/1234 5678\n").unwrap(),
            3
        );
    }

    #[test]
    fn rejects_short_loadavg() {
        assert!(parse_loadavg_runnable("0.42 0.36\n").is_err());
    }

    #[test]
    fn counts_cpus() {
        let stat = "cpu  1 2 3 4\ncpu0 1 2 3 4\ncpu1 1 2 3 4\nctxt 5\n";
        assert_eq!(count_cpus(stat), 2);
    }

    #[test]
    fn emits_raw_and_normalized() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stat"),
            "cpu  1 2 3 4\ncpu0 1 2 3 4\ncpu1 1 2 3 4\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("loadavg"), "0.5 0.4 0.3 5/100 200\n").unwrap();

        let mut collector = RunQueueCollector::new(dir.path());
        let mut out = Vec::new();
        collector.collect(Instant::now(), &mut out).unwrap();

        let value = |family: &str| out.iter().find(|s| s.key.family == family).unwrap().value;
        // 5 runnable minus the exporter itself = 4, over 2 CPUs.
        assert!((value("hires_run_queue_depth") - 4.0).abs() < 1e-9);
        assert!((value("hires_run_queue_per_cpu") - 2.0).abs() < 1e-9);
    }
}
