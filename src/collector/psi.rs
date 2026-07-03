//! Pressure Stall Information collector: `/proc/pressure/{cpu,memory,io}`.
//!
//! The kernel's own `avg10` is a 10-second moving average, which is exactly
//! the smoothing this exporter tries to avoid. Instead we derive the stall
//! share over each sampling interval from the cumulative `total`
//! (microseconds) counter.

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{
    CollectError, Collector, MetricDesc, MetricKey, RateTracker, SamplePoint, parse_field,
};

pub const DESCRIPTORS: &[MetricDesc] = &[
    MetricDesc {
        family: "hires_psi_cpu_some",
        help: "Share of interval at least one task stalled on CPU, percent.",
    },
    MetricDesc {
        family: "hires_psi_cpu_full",
        help: "Share of interval all non-idle tasks stalled on CPU, percent.",
    },
    MetricDesc {
        family: "hires_psi_memory_some",
        help: "Share of interval at least one task stalled on memory, percent.",
    },
    MetricDesc {
        family: "hires_psi_memory_full",
        help: "Share of interval all non-idle tasks stalled on memory, percent.",
    },
    MetricDesc {
        family: "hires_psi_io_some",
        help: "Share of interval at least one task stalled on I/O, percent.",
    },
    MetricDesc {
        family: "hires_psi_io_full",
        help: "Share of interval all non-idle tasks stalled on I/O, percent.",
    },
];

/// Cumulative stall totals (microseconds) from one PSI file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PsiTotals {
    pub some_total_us: u64,
    /// `full` is absent for the CPU resource on older kernels.
    pub full_total_us: Option<u64>,
}

/// Parse one PSI file, e.g.:
///
/// ```text
/// some avg10=0.00 avg60=0.00 avg300=0.00 total=123456
/// full avg10=0.00 avg60=0.00 avg300=0.00 total=12345
/// ```
pub fn parse_psi(raw: &str) -> Result<PsiTotals, CollectError> {
    let mut some = None;
    let mut full = None;
    for line in raw.lines() {
        let mut fields = line.split_ascii_whitespace();
        let kind = fields.next();
        let total = fields
            .find_map(|f| f.strip_prefix("total="))
            .ok_or_else(|| CollectError::Parse(format!("no total= in PSI line {line:?}")))?;
        let total: u64 = parse_field(total, "PSI total")?;
        match kind {
            Some("some") => some = Some(total),
            Some("full") => full = Some(total),
            _ => {}
        }
    }
    Ok(PsiTotals {
        some_total_us: some
            .ok_or_else(|| CollectError::Parse("no 'some' line in PSI file".into()))?,
        full_total_us: full,
    })
}

struct PsiResource {
    path: PathBuf,
    some_family: &'static str,
    full_family: &'static str,
    some_tracker: RateTracker,
    full_tracker: RateTracker,
}

pub struct PsiCollector {
    resources: Vec<PsiResource>,
}

impl PsiCollector {
    pub fn new(procfs_root: &Path) -> Self {
        let resource = |name: &str, some: &'static str, full: &'static str| PsiResource {
            path: procfs_root.join("pressure").join(name),
            some_family: some,
            full_family: full,
            some_tracker: RateTracker::default(),
            full_tracker: RateTracker::default(),
        };
        Self {
            resources: vec![
                resource("cpu", "hires_psi_cpu_some", "hires_psi_cpu_full"),
                resource("memory", "hires_psi_memory_some", "hires_psi_memory_full"),
                resource("io", "hires_psi_io_some", "hires_psi_io_full"),
            ],
        }
    }
}

impl Collector for PsiCollector {
    fn name(&self) -> &'static str {
        "psi"
    }

    fn descriptors(&self) -> &'static [MetricDesc] {
        DESCRIPTORS
    }

    fn collect(&mut self, now: Instant, out: &mut Vec<SamplePoint>) -> Result<(), CollectError> {
        for resource in &mut self.resources {
            let raw = std::fs::read_to_string(&resource.path).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    CollectError::Unavailable(format!(
                        "{} not found; kernel lacks PSI support (CONFIG_PSI)",
                        resource.path.display()
                    ))
                } else {
                    CollectError::Io(e)
                }
            })?;
            let totals = parse_psi(&raw)?;

            // total is in microseconds of stall time; rate (us/s) / 1e6 is the
            // stalled share of the interval, times 100 for percent.
            if let Some(rate) = resource.some_tracker.update(totals.some_total_us, now) {
                out.push(SamplePoint {
                    key: MetricKey::plain(resource.some_family),
                    value: (rate / 1e6 * 100.0).min(100.0),
                });
            }
            if let Some(full_total) = totals.full_total_us
                && let Some(rate) = resource.full_tracker.update(full_total, now)
            {
                out.push(SamplePoint {
                    key: MetricKey::plain(resource.full_family),
                    value: (rate / 1e6 * 100.0).min(100.0),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_psi_with_full() {
        let raw = "some avg10=0.12 avg60=0.05 avg300=0.01 total=123456\n\
                   full avg10=0.00 avg60=0.00 avg300=0.00 total=7890\n";
        let totals = parse_psi(raw).unwrap();
        assert_eq!(totals.some_total_us, 123456);
        assert_eq!(totals.full_total_us, Some(7890));
    }

    #[test]
    fn parses_psi_without_full() {
        let raw = "some avg10=0.00 avg60=0.00 avg300=0.00 total=42\n";
        let totals = parse_psi(raw).unwrap();
        assert_eq!(totals.some_total_us, 42);
        assert_eq!(totals.full_total_us, None);
    }

    fn write_psi(root: &Path, resource: &str, some_us: u64, full_us: u64) {
        std::fs::create_dir_all(root.join("pressure")).unwrap();
        std::fs::write(
            root.join("pressure").join(resource),
            format!(
                "some avg10=0.00 avg60=0.00 avg300=0.00 total={some_us}\n\
                 full avg10=0.00 avg60=0.00 avg300=0.00 total={full_us}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn computes_stall_percentages() {
        let dir = tempfile::tempdir().unwrap();
        for resource in ["cpu", "memory", "io"] {
            write_psi(dir.path(), resource, 1_000_000, 0);
        }
        let mut collector = PsiCollector::new(dir.path());
        let mut out = Vec::new();
        let t0 = Instant::now();
        collector.collect(t0, &mut out).unwrap();
        assert!(out.is_empty());

        // +200ms of 'some' CPU stall over a 1s interval => 20%.
        write_psi(dir.path(), "cpu", 1_200_000, 50_000);
        write_psi(dir.path(), "memory", 1_000_000, 0);
        write_psi(dir.path(), "io", 1_000_000, 0);
        collector
            .collect(t0 + Duration::from_secs(1), &mut out)
            .unwrap();

        let value = |family: &str| {
            out.iter()
                .find(|s| s.key.family == family)
                .unwrap_or_else(|| panic!("missing {family}"))
                .value
        };
        assert!((value("hires_psi_cpu_some") - 20.0).abs() < 1e-6);
        assert!((value("hires_psi_cpu_full") - 5.0).abs() < 1e-6);
        assert!(value("hires_psi_memory_some").abs() < 1e-6);
    }

    #[test]
    fn missing_psi_reports_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let mut collector = PsiCollector::new(dir.path());
        let mut out = Vec::new();
        match collector.collect(Instant::now(), &mut out) {
            Err(CollectError::Unavailable(_)) => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }
}
