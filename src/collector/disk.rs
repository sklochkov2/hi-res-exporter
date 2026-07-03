//! Disk collector: per-device I/O queue depth and utilization derived from
//! `/proc/diskstats` weighted-time counters.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config::DiskCollectorOptions;

use super::{CollectError, Collector, Label, MetricDesc, MetricKey, SamplePoint, parse_field};

pub const DESCRIPTORS: &[MetricDesc] = &[
    MetricDesc {
        family: "hires_io_queue_depth",
        help: "Average in-flight I/O requests (weighted queue time / elapsed).",
    },
    MetricDesc {
        family: "hires_io_util_percent",
        help: "Share of elapsed time the device had I/O in flight, percent.",
    },
];

/// Counters for one block device from `/proc/diskstats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskStats {
    pub name: String,
    /// Field 10: milliseconds spent doing I/O (device busy time).
    pub io_time_ms: u64,
    /// Field 11: weighted milliseconds spent doing I/O (sums per-request
    /// in-flight time, so delta/elapsed = average queue depth).
    pub weighted_io_time_ms: u64,
}

/// Parse `/proc/diskstats`. Returns every device line; filtering happens in
/// the collector.
pub fn parse_diskstats(raw: &str) -> Result<Vec<DiskStats>, CollectError> {
    let mut devices = Vec::new();
    for line in raw.lines() {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        if fields.is_empty() {
            continue;
        }
        // major minor name + at least the 11 classic stat fields.
        if fields.len() < 14 {
            return Err(CollectError::Parse(format!(
                "short /proc/diskstats line: {line:?}"
            )));
        }
        devices.push(DiskStats {
            name: fields[2].to_string(),
            io_time_ms: parse_field(fields[12], "/proc/diskstats io_time")?,
            weighted_io_time_ms: parse_field(fields[13], "/proc/diskstats weighted_io_time")?,
        });
    }
    Ok(devices)
}

/// Returns true for device names that are partitions of another device in
/// `names` (for example `sda1` for `sda`, `nvme0n1p2` for `nvme0n1`).
fn is_partition_of_listed(name: &str, names: &[&str]) -> bool {
    names.iter().any(|base| {
        if base.len() >= name.len() || !name.starts_with(base) {
            return false;
        }
        let suffix = &name[base.len()..];
        let suffix = suffix.strip_prefix('p').unwrap_or(suffix);
        !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Pseudo-devices that never map to real I/O latency.
fn is_pseudo_device(name: &str) -> bool {
    const PREFIXES: &[&str] = &["loop", "ram", "zram", "fd", "sr"];
    PREFIXES
        .iter()
        .any(|p| name.starts_with(p) && name[p.len()..].bytes().all(|b| b.is_ascii_digit()))
}

pub struct DiskCollector {
    diskstats_path: PathBuf,
    options: DiskCollectorOptions,
    prev: HashMap<String, (DiskStats, Instant)>,
}

impl DiskCollector {
    pub fn new(procfs_root: &Path, options: DiskCollectorOptions) -> Self {
        Self {
            diskstats_path: procfs_root.join("diskstats"),
            options,
            prev: HashMap::new(),
        }
    }

    fn device_selected(&self, name: &str, all_names: &[&str]) -> bool {
        if !self.options.include.is_empty() {
            return self.options.include.iter().any(|d| d == name);
        }
        if self.options.exclude.iter().any(|d| d == name) {
            return false;
        }
        !is_pseudo_device(name) && !is_partition_of_listed(name, all_names)
    }
}

impl Collector for DiskCollector {
    fn name(&self) -> &'static str {
        "disk"
    }

    fn descriptors(&self) -> &'static [MetricDesc] {
        DESCRIPTORS
    }

    fn collect(&mut self, now: Instant, out: &mut Vec<SamplePoint>) -> Result<(), CollectError> {
        let raw = std::fs::read_to_string(&self.diskstats_path)?;
        let devices = parse_diskstats(&raw)?;
        let all_names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();

        let mut total_queue_depth = 0.0;
        let mut saw_queue_depth = false;
        let mut tracked = 0usize;

        for device in &devices {
            if !self.device_selected(&device.name, &all_names) {
                continue;
            }
            tracked += 1;
            if tracked > self.options.max_devices {
                break;
            }
            let prev = self.prev.insert(device.name.clone(), (device.clone(), now));
            let Some((prev_stats, prev_time)) = prev else {
                continue;
            };
            let elapsed_ms = now.duration_since(prev_time).as_secs_f64() * 1000.0;
            if elapsed_ms <= 0.0
                || device.weighted_io_time_ms < prev_stats.weighted_io_time_ms
                || device.io_time_ms < prev_stats.io_time_ms
            {
                continue; // counter reset or clock anomaly; resume next tick
            }
            let queue_depth =
                (device.weighted_io_time_ms - prev_stats.weighted_io_time_ms) as f64 / elapsed_ms;
            let util = 100.0 * (device.io_time_ms - prev_stats.io_time_ms) as f64 / elapsed_ms;
            total_queue_depth += queue_depth;
            saw_queue_depth = true;

            let device_label = || vec![Label::new("device", device.name.clone())];
            out.push(SamplePoint {
                key: MetricKey::with_labels("hires_io_queue_depth", device_label()),
                value: queue_depth,
            });
            out.push(SamplePoint {
                key: MetricKey::with_labels("hires_io_util_percent", device_label()),
                value: util.min(100.0),
            });
        }

        // Drop state for devices that disappeared (hot-unplug).
        let live: Vec<String> = self
            .prev
            .keys()
            .filter(|name| !all_names.contains(&name.as_str()))
            .cloned()
            .collect();
        for name in live {
            self.prev.remove(&name);
        }

        if self.options.aggregate_all && saw_queue_depth {
            out.push(SamplePoint {
                key: MetricKey::with_labels(
                    "hires_io_queue_depth",
                    vec![Label::new("device", "__all__")],
                ),
                value: total_queue_depth,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SAMPLE: &str = "\
   8       0 sda 100 0 800 50 200 0 1600 100 0 120 180 0 0 0 0 0 0
   8       1 sda1 90 0 700 45 190 0 1500 95 0 110 170 0 0 0 0 0 0
 259       0 nvme0n1 500 0 4000 250 600 0 4800 300 2 400 700 0 0 0 0 0 0
 259       1 nvme0n1p1 480 0 3900 240 590 0 4700 290 2 390 680 0 0 0 0 0 0
   7       0 loop0 10 0 80 5 0 0 0 0 0 5 5 0 0 0 0 0 0
";

    #[test]
    fn parses_diskstats() {
        let devices = parse_diskstats(SAMPLE).unwrap();
        assert_eq!(devices.len(), 5);
        assert_eq!(devices[0].name, "sda");
        assert_eq!(devices[0].io_time_ms, 120);
        assert_eq!(devices[0].weighted_io_time_ms, 180);
    }

    #[test]
    fn detects_partitions_and_pseudo_devices() {
        let names = ["sda", "sda1", "nvme0n1", "nvme0n1p1"];
        assert!(is_partition_of_listed("sda1", &names));
        assert!(is_partition_of_listed("nvme0n1p1", &names));
        assert!(!is_partition_of_listed("sda", &names));
        assert!(!is_partition_of_listed("nvme0n1", &names));
        assert!(is_pseudo_device("loop0"));
        assert!(is_pseudo_device("zram0"));
        assert!(!is_pseudo_device("sda"));
        assert!(!is_pseudo_device("dm-0"));
    }

    fn write_diskstats(dir: &Path, sda_io: u64, sda_weighted: u64) {
        let contents = format!(
            "   8       0 sda 100 0 800 50 200 0 1600 100 0 {sda_io} {sda_weighted} 0 0 0 0 0 0\n\
             \u{20}  7       0 loop0 10 0 80 5 0 0 0 0 0 5 5 0 0 0 0 0 0\n"
        );
        std::fs::write(dir.join("diskstats"), contents).unwrap();
    }

    #[test]
    fn computes_queue_depth_and_util() {
        let dir = tempfile::tempdir().unwrap();
        let mut collector = DiskCollector::new(dir.path(), DiskCollectorOptions::default());
        let mut out = Vec::new();

        write_diskstats(dir.path(), 100, 200);
        let t0 = Instant::now();
        collector.collect(t0, &mut out).unwrap();
        assert!(out.is_empty());

        // +500ms busy, +2000ms weighted over 1s => util 50%, queue depth 2.
        write_diskstats(dir.path(), 600, 2200);
        collector
            .collect(t0 + Duration::from_secs(1), &mut out)
            .unwrap();

        let find = |family: &str, device: &str| {
            out.iter()
                .find(|s| s.key.family == family && s.key.labels.iter().any(|l| l.value == device))
                .unwrap_or_else(|| panic!("missing {family} for {device}"))
                .value
        };
        assert!((find("hires_io_queue_depth", "sda") - 2.0).abs() < 1e-9);
        assert!((find("hires_io_util_percent", "sda") - 50.0).abs() < 1e-9);
        assert!((find("hires_io_queue_depth", "__all__") - 2.0).abs() < 1e-9);
        assert!(
            !out.iter()
                .any(|s| s.key.labels.iter().any(|l| l.value == "loop0")),
            "pseudo devices must be filtered"
        );
    }

    #[test]
    fn include_list_limits_devices() {
        let dir = tempfile::tempdir().unwrap();
        let options = DiskCollectorOptions {
            include: vec!["loop0".into()],
            ..DiskCollectorOptions::default()
        };
        let mut collector = DiskCollector::new(dir.path(), options);
        let mut out = Vec::new();

        write_diskstats(dir.path(), 100, 200);
        let t0 = Instant::now();
        collector.collect(t0, &mut out).unwrap();
        write_diskstats(dir.path(), 600, 2200);
        collector
            .collect(t0 + Duration::from_secs(1), &mut out)
            .unwrap();

        // Only loop0 was included; its counters did not change.
        assert!(
            out.iter()
                .all(|s| s.key.labels.iter().all(|l| l.value != "sda"))
        );
    }
}
