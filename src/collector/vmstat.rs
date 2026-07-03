//! Vmstat collector: major page fault rate from `/proc/vmstat`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{
    CollectError, Collector, MetricDesc, MetricKey, RateTracker, SamplePoint, parse_field,
};

pub const DESCRIPTORS: &[MetricDesc] = &[MetricDesc {
    family: "hires_pgmajfault_per_sec",
    help: "Major page faults (disk-backed) per second.",
}];

/// Extract a single named counter from `/proc/vmstat`.
pub fn parse_vmstat_field(raw: &str, name: &str) -> Result<u64, CollectError> {
    for line in raw.lines() {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() == Some(name) {
            let value = fields
                .next()
                .ok_or_else(|| CollectError::Parse(format!("no value for vmstat {name}")))?;
            return parse_field(value, "vmstat value");
        }
    }
    Err(CollectError::Parse(format!("no {name} in /proc/vmstat")))
}

pub struct VmstatCollector {
    path: PathBuf,
    pgmajfault: RateTracker,
}

impl VmstatCollector {
    pub fn new(procfs_root: &Path) -> Self {
        Self {
            path: procfs_root.join("vmstat"),
            pgmajfault: RateTracker::default(),
        }
    }
}

impl Collector for VmstatCollector {
    fn name(&self) -> &'static str {
        "vmstat"
    }

    fn descriptors(&self) -> &'static [MetricDesc] {
        DESCRIPTORS
    }

    fn collect(&mut self, now: Instant, out: &mut Vec<SamplePoint>) -> Result<(), CollectError> {
        let raw = std::fs::read_to_string(&self.path)?;
        let value = parse_vmstat_field(&raw, "pgmajfault")?;
        if let Some(rate) = self.pgmajfault.update(value, now) {
            out.push(SamplePoint {
                key: MetricKey::plain("hires_pgmajfault_per_sec"),
                value: rate,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SAMPLE: &str = "nr_free_pages 100\npgfault 5000\npgmajfault 250\npgpgin 42\n";

    #[test]
    fn parses_named_field() {
        assert_eq!(parse_vmstat_field(SAMPLE, "pgmajfault").unwrap(), 250);
        assert!(parse_vmstat_field(SAMPLE, "nope").is_err());
    }

    #[test]
    fn emits_fault_rate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vmstat");
        std::fs::write(&path, "pgmajfault 100\n").unwrap();

        let mut collector = VmstatCollector::new(dir.path());
        let mut out = Vec::new();
        let t0 = Instant::now();
        collector.collect(t0, &mut out).unwrap();
        assert!(out.is_empty());

        std::fs::write(&path, "pgmajfault 150\n").unwrap();
        collector
            .collect(t0 + Duration::from_secs(2), &mut out)
            .unwrap();
        assert!((out[0].value - 25.0).abs() < 1e-9);
    }
}
