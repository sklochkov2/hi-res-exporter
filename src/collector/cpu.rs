//! CPU collector: aggregate CPU mode percentages plus context-switch and
//! interrupt rates, all derived from `/proc/stat`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{
    CollectError, Collector, MetricDesc, MetricKey, RateTracker, SamplePoint, parse_field,
};

pub const DESCRIPTORS: &[MetricDesc] = &[
    MetricDesc {
        family: "hires_cpu_user",
        help: "CPU time share spent in user+nice mode, percent of total.",
    },
    MetricDesc {
        family: "hires_cpu_system",
        help: "CPU time share spent in system+irq+softirq mode, percent of total.",
    },
    MetricDesc {
        family: "hires_cpu_iowait",
        help: "CPU time share spent in iowait, percent of total.",
    },
    MetricDesc {
        family: "hires_ctxt_switches_per_sec",
        help: "Context switches per second.",
    },
    MetricDesc {
        family: "hires_interrupts_per_sec",
        help: "Hardware interrupts serviced per second.",
    },
];

/// Cumulative jiffies by CPU mode from the aggregate `cpu` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimes {
    pub fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StatSnapshot {
    pub cpu: CpuTimes,
    pub ctxt: u64,
    pub intr: u64,
}

/// Parse the parts of `/proc/stat` we care about.
pub fn parse_proc_stat(raw: &str) -> Result<StatSnapshot, CollectError> {
    let mut cpu = None;
    let mut ctxt = None;
    let mut intr = None;

    for line in raw.lines() {
        let mut fields = line.split_ascii_whitespace();
        match fields.next() {
            Some("cpu") => {
                let mut next = |name: &str| -> Result<u64, CollectError> {
                    let raw = fields.next().ok_or_else(|| {
                        CollectError::Parse(format!("missing {name} field in /proc/stat cpu line"))
                    })?;
                    parse_field(raw, "/proc/stat cpu line")
                };
                cpu = Some(CpuTimes {
                    user: next("user")?,
                    nice: next("nice")?,
                    system: next("system")?,
                    idle: next("idle")?,
                    iowait: next("iowait")?,
                    irq: next("irq")?,
                    softirq: next("softirq")?,
                    // steal is absent on very old kernels; treat as zero.
                    steal: fields.next().map(|f| f.parse().unwrap_or(0)).unwrap_or(0),
                });
            }
            Some("ctxt") => {
                let raw = fields
                    .next()
                    .ok_or_else(|| CollectError::Parse("missing ctxt value".into()))?;
                ctxt = Some(parse_field(raw, "/proc/stat ctxt line")?);
            }
            Some("intr") => {
                // First field after "intr" is the total; per-IRQ counts follow.
                let raw = fields
                    .next()
                    .ok_or_else(|| CollectError::Parse("missing intr value".into()))?;
                intr = Some(parse_field(raw, "/proc/stat intr line")?);
            }
            _ => {}
        }
    }

    Ok(StatSnapshot {
        cpu: cpu
            .ok_or_else(|| CollectError::Parse("no aggregate cpu line in /proc/stat".into()))?,
        ctxt: ctxt.ok_or_else(|| CollectError::Parse("no ctxt line in /proc/stat".into()))?,
        intr: intr.ok_or_else(|| CollectError::Parse("no intr line in /proc/stat".into()))?,
    })
}

pub struct CpuCollector {
    stat_path: PathBuf,
    prev_cpu: Option<CpuTimes>,
    ctxt_rate: RateTracker,
    intr_rate: RateTracker,
}

impl CpuCollector {
    pub fn new(procfs_root: &Path) -> Self {
        Self {
            stat_path: procfs_root.join("stat"),
            prev_cpu: None,
            ctxt_rate: RateTracker::default(),
            intr_rate: RateTracker::default(),
        }
    }
}

impl Collector for CpuCollector {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn descriptors(&self) -> &'static [MetricDesc] {
        DESCRIPTORS
    }

    fn collect(&mut self, now: Instant, out: &mut Vec<SamplePoint>) -> Result<(), CollectError> {
        let raw = std::fs::read_to_string(&self.stat_path)?;
        let snapshot = parse_proc_stat(&raw)?;

        if let Some(prev) = self.prev_cpu.replace(snapshot.cpu) {
            let total_delta = snapshot.cpu.total().saturating_sub(prev.total());
            // Guard against counter resets and identical reads (short periods
            // relative to the kernel jiffy granularity).
            if snapshot.cpu.total() >= prev.total() && total_delta > 0 {
                let pct = |curr: u64, prev: u64| {
                    100.0 * curr.saturating_sub(prev) as f64 / total_delta as f64
                };
                let user = pct(snapshot.cpu.user + snapshot.cpu.nice, prev.user + prev.nice);
                let system = pct(
                    snapshot.cpu.system + snapshot.cpu.irq + snapshot.cpu.softirq,
                    prev.system + prev.irq + prev.softirq,
                );
                let iowait = pct(snapshot.cpu.iowait, prev.iowait);
                out.push(SamplePoint {
                    key: MetricKey::plain("hires_cpu_user"),
                    value: user,
                });
                out.push(SamplePoint {
                    key: MetricKey::plain("hires_cpu_system"),
                    value: system,
                });
                out.push(SamplePoint {
                    key: MetricKey::plain("hires_cpu_iowait"),
                    value: iowait,
                });
            }
        }

        if let Some(rate) = self.ctxt_rate.update(snapshot.ctxt, now) {
            out.push(SamplePoint {
                key: MetricKey::plain("hires_ctxt_switches_per_sec"),
                value: rate,
            });
        }
        if let Some(rate) = self.intr_rate.update(snapshot.intr, now) {
            out.push(SamplePoint {
                key: MetricKey::plain("hires_interrupts_per_sec"),
                value: rate,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
cpu  1000 50 300 8000 200 10 40 5 0 0
cpu0 500 25 150 4000 100 5 20 2 0 0
cpu1 500 25 150 4000 100 5 20 3 0 0
intr 123456 0 12 0
ctxt 654321
btime 1700000000
processes 4242
procs_running 3
procs_blocked 1
";

    #[test]
    fn parses_proc_stat() {
        let snapshot = parse_proc_stat(SAMPLE).unwrap();
        assert_eq!(snapshot.cpu.user, 1000);
        assert_eq!(snapshot.cpu.nice, 50);
        assert_eq!(snapshot.cpu.system, 300);
        assert_eq!(snapshot.cpu.idle, 8000);
        assert_eq!(snapshot.cpu.iowait, 200);
        assert_eq!(snapshot.cpu.irq, 10);
        assert_eq!(snapshot.cpu.softirq, 40);
        assert_eq!(snapshot.cpu.steal, 5);
        assert_eq!(snapshot.ctxt, 654321);
        assert_eq!(snapshot.intr, 123456);
    }

    #[test]
    fn parses_stat_without_steal() {
        let raw = "cpu  100 0 50 800 25 5 10\nintr 1 0\nctxt 2\n";
        let snapshot = parse_proc_stat(raw).unwrap();
        assert_eq!(snapshot.cpu.steal, 0);
    }

    #[test]
    fn rejects_stat_without_cpu_line() {
        let raw = "intr 1 0\nctxt 2\n";
        assert!(parse_proc_stat(raw).is_err());
    }

    #[test]
    fn computes_mode_percentages_between_samples() {
        let dir = tempfile::tempdir().unwrap();
        let stat = dir.path().join("stat");
        let mut collector = CpuCollector::new(dir.path());
        let mut out = Vec::new();

        std::fs::write(
            &stat,
            "cpu  100 0 100 700 100 0 0 0\nintr 1000 0\nctxt 2000\n",
        )
        .unwrap();
        let t0 = Instant::now();
        collector.collect(t0, &mut out).unwrap();
        assert!(out.is_empty(), "first sample must not emit rate metrics");

        // +50 user, +25 system, +10 iowait, +115 idle => total delta 200.
        std::fs::write(
            &stat,
            "cpu  150 0 125 815 110 0 0 0\nintr 2000 0\nctxt 4000\n",
        )
        .unwrap();
        collector
            .collect(t0 + std::time::Duration::from_secs(1), &mut out)
            .unwrap();

        let value = |family: &str| {
            out.iter()
                .find(|s| s.key.family == family)
                .unwrap_or_else(|| panic!("missing {family}"))
                .value
        };
        assert!((value("hires_cpu_user") - 25.0).abs() < 1e-9);
        assert!((value("hires_cpu_system") - 12.5).abs() < 1e-9);
        assert!((value("hires_cpu_iowait") - 5.0).abs() < 1e-9);
        assert!((value("hires_ctxt_switches_per_sec") - 2000.0).abs() < 1e-6);
        assert!((value("hires_interrupts_per_sec") - 1000.0).abs() < 1e-6);
    }
}
