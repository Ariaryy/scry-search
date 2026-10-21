//! Daemon command-line options and benchmark mode validation.

pub(super) struct BenchmarkSnapshot {
    pub(super) path: std::path::PathBuf,
    pub(super) overlay: Option<std::path::PathBuf>,
}

pub(super) struct StartupOptions {
    pub(super) volumes: Vec<String>,
    pub(super) index_mbps: Option<u64>,
    pub(super) benchmark_snapshot: Vec<BenchmarkSnapshot>,
    pub(super) benchmark_pipe: Option<String>,
    pub(super) benchmark_cache: Option<std::path::PathBuf>,
}

pub(super) fn startup_options() -> anyhow::Result<StartupOptions> {
    let mut volumes = Vec::new();
    let mut index_mbps = None;
    let mut benchmark_snapshot: Vec<BenchmarkSnapshot> = Vec::new();
    let mut benchmark_pipe = None;
    let mut benchmark_cache = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "scryd {}\n\nUsage: scryd [OPTIONS] [VOLUME ...]\n\n  --index-mbps N  Cap aggregate index reads in MiB/s\n  --unbounded     Disable the index read cap\n  -h, --help      Print help\n  -V, --version   Print version",
                    env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("scryd {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--unbounded" => index_mbps = Some(0),
            "--benchmark-snapshot" => {
                benchmark_snapshot.push(BenchmarkSnapshot {
                    path: args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--benchmark-snapshot needs a file"))?
                        .into(),
                    overlay: None,
                });
            }
            "--benchmark-overlay" => {
                let snapshot = benchmark_snapshot
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("overlay requires a preceding snapshot"))?;
                anyhow::ensure!(snapshot.overlay.is_none(), "one overlay per snapshot");
                snapshot.overlay = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("overlay needs a file"))?
                        .into(),
                );
            }
            "--benchmark-live-cache" => {
                benchmark_cache = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("live benchmark cache needs a directory"))?
                        .into(),
                );
            }
            "--benchmark-pipe" => {
                benchmark_pipe = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--benchmark-pipe needs a pipe name"))?,
                );
            }
            "--index-mbps" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--index-mbps requires a value"))?;
                index_mbps =
                    Some(value.parse().map_err(|_| {
                        anyhow::anyhow!("--index-mbps expects a non-negative integer")
                    })?);
            }
            _ if arg.starts_with("--index-mbps=") => {
                let value = arg.trim_start_matches("--index-mbps=");
                index_mbps =
                    Some(value.parse().map_err(|_| {
                        anyhow::anyhow!("--index-mbps expects a non-negative integer")
                    })?);
            }
            _ if arg.starts_with('-') => return Err(anyhow::anyhow!("unknown option: {arg}")),
            _ => volumes.push(arg),
        }
    }
    anyhow::ensure!(
        (!benchmark_snapshot.is_empty() || benchmark_cache.is_some()) == benchmark_pipe.is_some(),
        "benchmark snapshots or live cache require a private pipe"
    );
    anyhow::ensure!(
        benchmark_snapshot.is_empty()
            || (volumes.is_empty() && index_mbps.is_none() && benchmark_cache.is_none()),
        "frozen benchmark mode cannot ingest volumes"
    );
    anyhow::ensure!(
        benchmark_snapshot.len() <= 16,
        "at most sixteen benchmark volumes"
    );
    anyhow::ensure!(
        benchmark_cache.is_none() || !volumes.is_empty(),
        "live benchmark requires explicit volumes"
    );
    if let Some(pipe) = &benchmark_pipe {
        anyhow::ensure!(
            pipe.starts_with(r"\\.\pipe\scry-bench-") && pipe.len() <= 200,
            "benchmark mode requires a distinct local scry-bench- pipe"
        );
    }
    Ok(StartupOptions {
        volumes,
        index_mbps,
        benchmark_snapshot,
        benchmark_pipe,
        benchmark_cache,
    })
}
