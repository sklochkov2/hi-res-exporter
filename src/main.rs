use std::path::PathBuf;
use std::sync::Arc;

use tracing::info;

use hi_res_exporter::collector::build_collectors;
use hi_res_exporter::config::Config;
use hi_res_exporter::sampler::Sampler;
use hi_res_exporter::server;
use hi_res_exporter::snapshot::SnapshotStore;

const USAGE: &str = "\
hi-res-exporter: high-resolution Linux load Prometheus exporter

Usage:
  hi-res-exporter [--config <path>]

Options:
  --config <path>  TOML configuration file (defaults are used if omitted)
  --help           Show this help

Environment overrides:
  HIRES_LISTEN            server.listen
  HIRES_SAMPLING_PERIOD   sampling.period (e.g. \"200ms\")
  HIRES_PROCFS_ROOT       procfs_root (testing)
";

fn parse_args() -> Result<Option<PathBuf>, String> {
    let mut config_path = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                let value = args.next().ok_or("--config requires a path")?;
                config_path = Some(PathBuf::from(value));
            }
            "--help" | "-h" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(config_path)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let config_path = match parse_args() {
        Ok(path) => path,
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let config = match Config::load(config_path.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
    };

    let store = Arc::new(SnapshotStore::new());
    let collectors = build_collectors(&config);
    info!(
        collectors = ?collectors.iter().map(|c| c.name()).collect::<Vec<_>>(),
        procfs_root = %config.procfs_root.display(),
        "starting hi-res-exporter"
    );

    let listen = config.server.listen.clone();
    let sampler = Sampler::new(config, collectors, Arc::clone(&store));

    let sampler_task = tokio::spawn(sampler.run());
    let server_task = tokio::spawn(async move {
        if let Err(error) = server::serve(&listen, store).await {
            eprintln!("http server failed: {error}");
            std::process::exit(1);
        }
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("received ctrl-c, shutting down");
        }
        _ = sampler_task => {
            eprintln!("sampler task exited unexpectedly");
            std::process::exit(1);
        }
        _ = server_task => {}
    }
}
