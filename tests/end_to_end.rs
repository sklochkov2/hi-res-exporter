//! End-to-end test: a fake procfs tree is sampled through the real
//! collector/aggregator/snapshot pipeline and scraped over HTTP.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hi_res_exporter::collector::build_collectors;
use hi_res_exporter::config::Config;
use hi_res_exporter::sampler::Sampler;
use hi_res_exporter::server;
use hi_res_exporter::snapshot::SnapshotStore;

/// Write a consistent fake procfs tree. `step` advances all counters so
/// consecutive samples produce non-trivial deltas.
fn write_fake_procfs(root: &Path, step: u64) {
    std::fs::create_dir_all(root.join("pressure")).unwrap();
    std::fs::create_dir_all(root.join("net")).unwrap();

    let user = 1000 + 50 * step;
    let system = 300 + 20 * step;
    let iowait = 200 + 10 * step;
    let idle = 8000 + 120 * step;
    std::fs::write(
        root.join("stat"),
        format!(
            "cpu  {user} 0 {system} {idle} {iowait} 0 0 0 0 0\n\
             cpu0 0 0 0 0 0 0 0 0 0 0\n\
             cpu1 0 0 0 0 0 0 0 0 0 0\n\
             intr {} 0\nctxt {}\n",
            10_000 + 500 * step,
            20_000 + 1_000 * step
        ),
    )
    .unwrap();

    std::fs::write(
        root.join("diskstats"),
        format!(
            "   8       0 sda 100 0 800 50 200 0 1600 100 0 {} {} 0 0 0 0 0 0\n",
            1_000 + 100 * step,
            2_000 + 400 * step
        ),
    )
    .unwrap();

    for (resource, some_step, full_step) in [
        ("cpu", 30_000, 0),
        ("memory", 10_000, 5_000),
        ("io", 20_000, 8_000),
    ] {
        std::fs::write(
            root.join("pressure").join(resource),
            format!(
                "some avg10=0.00 avg60=0.00 avg300=0.00 total={}\n\
                 full avg10=0.00 avg60=0.00 avg300=0.00 total={}\n",
                1_000_000 + some_step * step,
                500_000 + full_step * step
            ),
        )
        .unwrap();
    }

    std::fs::write(root.join("loadavg"), "0.50 0.40 0.30 3/500 12345\n").unwrap();
    std::fs::write(
        root.join("net").join("softnet_stat"),
        format!(
            "{:08x} {:08x} {:08x}\n",
            0x1000 + 0x100 * step,
            2 * step,
            step
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("vmstat"),
        format!("pgfault 100\npgmajfault {}\n", 40 * step),
    )
    .unwrap();
}

fn test_config(procfs_root: &Path) -> Config {
    let raw = format!(
        r#"
            procfs_root = "{}"

            [sampling]
            period = "100ms"

            [aggregation]
            windows = ["10s", "60s"]
            quantiles = [0.5, 0.99]
        "#,
        procfs_root.display()
    );
    let config: Config = toml::from_str(&raw).unwrap();
    config.validate().unwrap();
    config
}

/// Drive the sampler synchronously over a simulated timeline and return the
/// rendered exposition text.
fn run_pipeline(dir: &Path) -> String {
    let config = test_config(dir);
    let store = Arc::new(SnapshotStore::new());
    let collectors = build_collectors(&config);
    assert_eq!(collectors.len(), 6, "all collectors should be enabled");

    let mut sampler = Sampler::new(config, collectors, Arc::clone(&store));
    let t0 = Instant::now();
    for step in 0..5u64 {
        write_fake_procfs(dir, step);
        sampler.sample_once(t0 + Duration::from_millis(100 * step));
    }
    sampler.publish_snapshot(t0 + Duration::from_millis(500));
    assert!(store.is_ready());
    store.latest().as_ref().clone()
}

#[test]
fn pipeline_produces_expected_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let text = run_pipeline(dir.path());

    // Core gauges from the design's metric schema.
    for family in [
        "hires_cpu_user_percentile",
        "hires_cpu_system_percentile",
        "hires_cpu_iowait_percentile",
        "hires_io_queue_depth_percentile",
        "hires_psi_cpu_some_percentile",
        "hires_psi_memory_full_percentile",
        "hires_psi_io_some_percentile",
        "hires_run_queue_depth_percentile",
        "hires_ctxt_switches_per_sec_percentile",
        "hires_interrupts_per_sec_percentile",
        "hires_softnet_drops_per_sec_percentile",
        "hires_softnet_time_squeezed_per_sec_percentile",
        "hires_pgmajfault_per_sec_percentile",
    ] {
        assert!(text.contains(family), "missing {family} in:\n{text}");
    }

    // Labels: window and quantile on every percentile line, device on I/O.
    assert!(text.contains(r#"hires_cpu_user_percentile{window="10s",quantile="0.5"}"#));
    assert!(text.contains(r#"hires_cpu_user_percentile{window="1m",quantile="0.99"}"#));
    assert!(text.contains(r#"hires_io_queue_depth_percentile{device="sda",window="10s""#));
    assert!(text.contains(r#"hires_io_queue_depth_percentile{device="__all__",window="10s""#));

    // Self metrics.
    assert!(text.contains(r#"hires_exporter_collector_enabled{collector="cpu"} 1"#));
    assert!(text.contains(r#"hires_exporter_samples_total{collector="cpu"}"#));
    assert!(text.contains("hires_exporter_window_series"));
    assert!(text.contains("hires_exporter_build_info"));

    // Value sanity: CPU deltas are constant per step, so every quantile of
    // user% is 50/200 of total (user 50 / total 200 jiffies) = 25%.
    let user_p50 = extract_value(
        &text,
        r#"hires_cpu_user_percentile{window="10s",quantile="0.5"} "#,
    );
    assert!((user_p50 - 25.0).abs() < 0.5, "user p50 = {user_p50}");
    // Queue depth: +400ms weighted per 100ms elapsed = 4.
    let queue_p50 = extract_value(
        &text,
        r#"hires_io_queue_depth_percentile{device="sda",window="10s",quantile="0.5"} "#,
    );
    assert!((queue_p50 - 4.0).abs() < 0.1, "queue p50 = {queue_p50}");
}

#[test]
fn missing_psi_disables_only_psi_collector() {
    let dir = tempfile::tempdir().unwrap();
    write_fake_procfs(dir.path(), 0);
    std::fs::remove_dir_all(dir.path().join("pressure")).unwrap();

    let config = test_config(dir.path());
    let store = Arc::new(SnapshotStore::new());
    let collectors = build_collectors(&config);
    let mut sampler = Sampler::new(config, collectors, store.clone());

    let t0 = Instant::now();
    for step in 0..3u64 {
        write_fake_procfs(dir.path(), step);
        std::fs::remove_dir_all(dir.path().join("pressure")).unwrap();
        sampler.sample_once(t0 + Duration::from_millis(100 * step));
    }
    sampler.publish_snapshot(t0 + Duration::from_millis(300));

    let text = store.latest().as_ref().clone();
    assert!(text.contains(r#"hires_exporter_collector_enabled{collector="psi"} 0"#));
    assert!(text.contains(r#"hires_exporter_collector_enabled{collector="cpu"} 1"#));
    assert!(text.contains("hires_cpu_user_percentile"));
    assert!(!text.contains("hires_psi_cpu_some_percentile{"));
}

#[tokio::test]
async fn http_endpoints_serve_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let text = run_pipeline(dir.path());

    let store = Arc::new(SnapshotStore::new());
    store.publish(text);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, hi_res_exporter::server::router(store))
            .await
            .unwrap();
    });

    let metrics = http_get(addr, "/metrics").await;
    assert!(metrics.contains("hires_cpu_user_percentile"));
    let health = http_get(addr, "/healthz").await;
    assert!(health.contains("ok"));
    let ready = http_get(addr, "/readyz").await;
    assert!(ready.contains("ready"));

    // readiness is negative before the first snapshot
    let empty_store = Arc::new(SnapshotStore::new());
    let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener2.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener2, server::router(empty_store))
            .await
            .unwrap();
    });
    let not_ready = http_get_raw(addr2, "/readyz").await;
    assert!(not_ready.starts_with("HTTP/1.1 503"), "{not_ready}");
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    let raw = http_get_raw(addr, path).await;
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    raw.split_once("\r\n\r\n").unwrap().1.to_string()
}

async fn http_get_raw(addr: std::net::SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nhost: test\r\nconnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8(buf).unwrap()
}

fn extract_value(text: &str, prefix: &str) -> f64 {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no line with prefix {prefix:?} in:\n{text}"))
        .trim()
        .parse()
        .unwrap()
}
