//! Softnet collector: network receive-path pressure from
//! `/proc/net/softnet_stat` (per-CPU hex counters, summed host-wide).

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{CollectError, Collector, MetricDesc, MetricKey, RateTracker, SamplePoint};

pub const DESCRIPTORS: &[MetricDesc] = &[
    MetricDesc {
        family: "hires_softnet_processed_per_sec",
        help: "Packets processed by the softnet backlog per second.",
    },
    MetricDesc {
        family: "hires_softnet_drops_per_sec",
        help: "Packets dropped due to full softnet backlog per second.",
    },
    MetricDesc {
        family: "hires_softnet_time_squeezed_per_sec",
        help: "net_rx_action budget/time exhaustions per second.",
    },
];

/// Host-wide sums of the softnet counters we track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SoftnetTotals {
    pub processed: u64,
    pub dropped: u64,
    pub time_squeezed: u64,
}

/// Parse `/proc/net/softnet_stat`: one row of hex fields per CPU; columns
/// are processed, dropped, time_squeeze, ...
pub fn parse_softnet(raw: &str) -> Result<SoftnetTotals, CollectError> {
    let mut totals = SoftnetTotals::default();
    for line in raw.lines() {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        if fields.is_empty() {
            continue;
        }
        if fields.len() < 3 {
            return Err(CollectError::Parse(format!(
                "short softnet_stat line: {line:?}"
            )));
        }
        let hex = |raw: &str| -> Result<u64, CollectError> {
            u64::from_str_radix(raw, 16)
                .map_err(|_| CollectError::Parse(format!("bad hex field {raw:?} in softnet_stat")))
        };
        totals.processed += hex(fields[0])?;
        totals.dropped += hex(fields[1])?;
        totals.time_squeezed += hex(fields[2])?;
    }
    Ok(totals)
}

pub struct SoftnetCollector {
    path: PathBuf,
    processed: RateTracker,
    dropped: RateTracker,
    time_squeezed: RateTracker,
}

impl SoftnetCollector {
    pub fn new(procfs_root: &Path) -> Self {
        Self {
            path: procfs_root.join("net").join("softnet_stat"),
            processed: RateTracker::default(),
            dropped: RateTracker::default(),
            time_squeezed: RateTracker::default(),
        }
    }
}

impl Collector for SoftnetCollector {
    fn name(&self) -> &'static str {
        "softnet"
    }

    fn descriptors(&self) -> &'static [MetricDesc] {
        DESCRIPTORS
    }

    fn collect(&mut self, now: Instant, out: &mut Vec<SamplePoint>) -> Result<(), CollectError> {
        let raw = std::fs::read_to_string(&self.path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CollectError::Unavailable(format!("{} not found", self.path.display()))
            } else {
                CollectError::Io(e)
            }
        })?;
        let totals = parse_softnet(&raw)?;

        let mut push = |family: &'static str, rate: Option<f64>| {
            if let Some(rate) = rate {
                out.push(SamplePoint {
                    key: MetricKey::plain(family),
                    value: rate,
                });
            }
        };
        push(
            "hires_softnet_processed_per_sec",
            self.processed.update(totals.processed, now),
        );
        push(
            "hires_softnet_drops_per_sec",
            self.dropped.update(totals.dropped, now),
        );
        push(
            "hires_softnet_time_squeezed_per_sec",
            self.time_squeezed.update(totals.time_squeezed, now),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_and_sums_rows() {
        let raw = "\
0000006e 00000001 00000002 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000
000000c8 00000003 00000004 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000001
";
        let totals = parse_softnet(raw).unwrap();
        assert_eq!(totals.processed, 0x6e + 0xc8);
        assert_eq!(totals.dropped, 4);
        assert_eq!(totals.time_squeezed, 6);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_softnet("zz yy xx\n").is_err());
    }

    #[test]
    fn emits_rates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("net")).unwrap();
        let path = dir.path().join("net").join("softnet_stat");

        std::fs::write(&path, "00000064 00000000 00000000\n").unwrap();
        let mut collector = SoftnetCollector::new(dir.path());
        let mut out = Vec::new();
        let t0 = Instant::now();
        collector.collect(t0, &mut out).unwrap();
        assert!(out.is_empty());

        // +0x64 (100) processed, +10 dropped over 1s.
        std::fs::write(&path, "000000c8 0000000a 00000000\n").unwrap();
        collector
            .collect(t0 + Duration::from_secs(1), &mut out)
            .unwrap();

        let value = |family: &str| out.iter().find(|s| s.key.family == family).unwrap().value;
        assert!((value("hires_softnet_processed_per_sec") - 100.0).abs() < 1e-6);
        assert!((value("hires_softnet_drops_per_sec") - 10.0).abs() < 1e-6);
        assert!(value("hires_softnet_time_squeezed_per_sec").abs() < 1e-6);
    }
}
