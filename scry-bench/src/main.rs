//! Non-shipping, end-to-end baseline runner. Timed measurements use supported
//! client APIs; offline modules generate fixtures and independently count strata.
mod extension_counts;
mod fingerprint;
#[cfg(test)]
mod fingerprint_tests;
mod generate;
#[cfg(test)]
mod generate_tests;
mod observe;
mod scenario;
mod synthetic;
mod validation;
mod workload;

use anyhow::{bail, ensure, Context, Result};
use scry_client::{Client, ResultEntry, SearchExecution, SearchSession};
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use validation::Reference;
use workload::{Case, Workload};

const HELP: &str = "scry-bench [OPTIONS]
  synthesize           Generate an additional calibrated synthetic snapshot
  generate-workload    Generate private seeded queries and exact count strata
  fingerprint          Describe a snapshot with aggregate B2 metrics
  scenario             Create synthetic multi-volume/delta fixtures
  observe              Measure live query memory, idle counters and visibility
  SUBCOMMAND --help    Show subcommand options

  --workload FILE       JSON workload (default: built-in synthetic vocabulary)
  --snapshot FILE       Reference snapshot; repeat for at most sixteen volumes
  --overlay FILE        Static overlay for the preceding snapshot
  --cache-diagnostics BOOL  Read per-session cache counters outside query timers
  --output FILE         New JSONL file inside .scratch/ (never overwritten)
  --samples N           Warm sequences per case/route/round (default 40)
  --warmups N           Unrecorded sequences after first request (default 2)
  --rounds N            Independent connections, alternating route order (default 3)
  --limit N             Result limit, 1..20000 (default 50)
  --pipe NAME           Override daemon pipe for isolated fixtures
  --daemon-build LABEL  Daemon commit/profile; default unknown
  --machine LABEL       Generic machine label; default unknown
  --power LABEL         AC/battery and power plan; default unknown
  --help                Show help

Measurements include response decoding and bounded path materialization.
First request means a fresh connection, NOT cold OS cache or fresh daemon.
Queries/results are never printed. Output contains aggregate labels and counts.
Superseding sequences exercise RPC only; local search is synchronous.
Use scripts/benchmark-summary.py for summaries and explicit comparison gates.";

struct Config {
    workload: Workload,
    workload_hash: String,
    output: PathBuf,
    samples: u32,
    warmups: u32,
    rounds: u32,
    limit: u32,
    pipe: Option<String>,
    daemon_build: String,
    machine: String,
    power: String,
    reference: Vec<scry_core::IndexView>,
    reference_sha256: Option<String>,
    fingerprint: Option<serde_json::Value>,
    cache_diagnostics: bool,
}

impl Config {
    fn parse() -> Result<Option<Self>> {
        let mut workload_path = None;
        let mut output = PathBuf::from(".scratch/benchmark/baseline.jsonl");
        let (mut samples, mut warmups, mut rounds, mut limit) = (40, 2, 3, 50);
        let mut pipe = None;
        let mut daemon_build = "unknown".to_owned();
        let mut machine = "unknown".to_owned();
        let mut power = "unknown".to_owned();
        let mut snapshots: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
        let mut cache_diagnostics = false;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if matches!(arg.as_str(), "--help" | "-h") {
                println!("{HELP}");
                return Ok(None);
            }
            let value = args.next().context("option needs a value")?;
            match arg.as_str() {
                "--workload" => workload_path = Some(value),
                "--snapshot" => snapshots.push((PathBuf::from(value), None)),
                "--overlay" => {
                    let snapshot = snapshots
                        .last_mut()
                        .context("overlay requires a preceding snapshot")?;
                    ensure!(snapshot.1.is_none(), "one overlay per snapshot");
                    snapshot.1 = Some(value.into());
                }
                "--output" => output = PathBuf::from(value),
                "--samples" => samples = value.parse().context("invalid sample count")?,
                "--warmups" => warmups = value.parse().context("invalid warmup count")?,
                "--rounds" => rounds = value.parse().context("invalid round count")?,
                "--limit" => limit = value.parse().context("invalid limit")?,
                "--pipe" => pipe = Some(value),
                "--daemon-build" => daemon_build = value,
                "--machine" => machine = value,
                "--power" => power = value,
                "--cache-diagnostics" => {
                    cache_diagnostics = value
                        .parse()
                        .context("cache diagnostics expects true or false")?
                }
                _ => bail!("unknown option"),
            }
        }
        ensure!((1..=10_000).contains(&samples), "samples must be 1..10000");
        ensure!(warmups <= 100, "warmups must be 0..100");
        ensure!((1..=100).contains(&rounds), "rounds must be 1..100");
        ensure!((1..=20_000).contains(&limit), "limit must be 1..20000");
        let source = match workload_path {
            Some(path) => {
                use std::io::Read;
                let mut source = Vec::new();
                std::fs::File::open(path)
                    .context("opening workload")?
                    .take(1024 * 1024 + 1)
                    .read_to_end(&mut source)?;
                source
            }
            None => include_bytes!("../workload.example.json").to_vec(),
        };
        ensure!(source.len() <= 1024 * 1024, "workload exceeds 1 MiB");
        // A deterministic workload identity, not a hash of result paths.
        let hash = source.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
        let workload: Workload = serde_json::from_slice(&source).map_err(|_| {
            anyhow::anyhow!("invalid workload JSON (details omitted to keep queries private)")
        })?;
        workload.validate()?;
        let (reference, reference_sha256) = scenario::load(&snapshots)?;
        Ok(Some(Self {
            workload,
            workload_hash: format!("{hash:016x}"),
            output,
            samples,
            warmups,
            rounds,
            limit,
            pipe,
            daemon_build,
            machine,
            power,
            reference,
            reference_sha256,
            fingerprint: None,
            cache_diagnostics,
        }))
    }

    fn connect(&self) -> Result<Client> {
        // A live daemon creates pipe instances serially. Only retry for this
        // short connection race, outside query timing. Do not log pipe names.
        let start = Instant::now();
        loop {
            let result = match &self.pipe {
                Some(pipe) => Client::connect_to(pipe),
                None => Client::connect(),
            };
            match result {
                Ok(client) => return Ok(client),
                Err(_) if start.elapsed() < Duration::from_secs(2) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => bail!("daemon unavailable after 2 seconds"),
            }
        }
    }

    fn writer(&self) -> Result<BufWriter<std::fs::File>> {
        scratch_writer(&self.output)
    }
}

fn prepare_scratch_output(output: &std::path::Path) -> Result<()> {
    // Require local raw captures to be under the ignored scratch root.
    // Canonicalizing catches '..' and junction escapes in parent paths.
    let scratch = std::env::current_dir()?.join(".scratch");
    fs::create_dir_all(&scratch)?;
    let scratch = scratch.canonicalize()?;
    let parent = output.parent().context("output needs a parent directory")?;
    // Reject lexical escapes before creating any directories, then check
    // canonical ancestry as well (including existing junctions).
    let absolute = std::path::absolute(output)?;
    ensure!(
        absolute.starts_with(std::env::current_dir()?.join(".scratch")),
        "output must be inside .scratch/"
    );
    fs::create_dir_all(parent)?;
    ensure!(
        parent.canonicalize()?.starts_with(scratch),
        "output must be inside .scratch/"
    );
    ensure!(!output.exists(), "output already exists");
    Ok(())
}

fn scratch_writer(output: &std::path::Path) -> Result<BufWriter<std::fs::File>> {
    prepare_scratch_output(output)?;
    Ok(BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)
            .context("creating output (choose a new filename for each run)")?,
    ))
}

enum Runner {
    Rpc(SearchSession),
    Local(Client),
}

impl Runner {
    fn new(config: &Config, case: &Case, local: bool) -> Result<Self> {
        let client = config.connect()?;
        if local {
            Ok(Self::Local(client))
        } else {
            let mut session = client.into_search_session(config.limit);
            session.set_order(case.order()?);
            Ok(Self::Rpc(session))
        }
    }

    fn query(
        &mut self,
        case: &Case,
        pattern: &str,
        limit: u32,
    ) -> Result<(Vec<ResultEntry>, &'static str)> {
        match self {
            Self::Rpc(session) => {
                session.submit(case.kind()?, pattern)?;
                Ok((session.wait_latest()?, "rpc"))
            }
            Self::Local(client) => {
                let (entries, execution) = client.search_local_ordered_with_execution(
                    case.kind()?,
                    pattern,
                    limit,
                    case.order()?,
                )?;
                Ok((
                    entries,
                    match execution {
                        SearchExecution::SharedIndex => "shared_index",
                        SearchExecution::RpcFallback => "rpc_fallback",
                    },
                ))
            }
        }
    }
}

fn write_row(out: &mut impl Write, row: serde_json::Value) -> Result<()> {
    serde_json::to_writer(&mut *out, &row)?;
    writeln!(out)?;
    Ok(())
}

fn refinement_diagnostics(stats: &str) -> Result<serde_json::Value> {
    let field = |key: &str| {
        stats
            .split_whitespace()
            .find_map(|word| word.strip_prefix(&format!("{key}=")))
    };
    ensure!(
        field("refinement_diagnostics") == Some("1"),
        "daemon lacks per-request refinement diagnostics"
    );
    Ok(
        json!({"reused_volumes":field("last_reused_volumes").context("missing reused count")?.parse::<u64>()?,"scanned_volumes":field("last_scanned_volumes").context("missing scan count")?.parse::<u64>()?,"bypassed":field("last_bypassed").context("missing bypass flag")?.parse::<bool>()?,"cancelled":field("last_cancelled").context("missing cancellation flag")?.parse::<bool>()?}),
    )
}

/// Untimed ordered, full-entry equality against independent RPC. This catches
/// equal-count/wrong-identity and ranking bugs. A changing index can fail it;
/// fail the capture explicitly rather than turning that into a timing result.
fn verify(config: &Config, case: &Case, local: bool) -> Result<Vec<Reference>> {
    let mut runner = Runner::new(config, case, local)?;
    let oracle = config.connect()?;
    let mut references = Vec::with_capacity(case.patterns.len());
    for pattern in &case.patterns {
        let entries = reference_entries(config, case, pattern, &oracle)?;
        references.push(Reference::of(&entries));
    }
    if case.supersede {
        let Runner::Rpc(session) = &mut runner else {
            unreachable!()
        };
        for pattern in &case.patterns {
            session.submit(case.kind()?, pattern)?;
        }
        let actual = session.wait_latest()?;
        let expected = reference_entries(config, case, case.patterns.last().unwrap(), &oracle)?;
        ensure!(
            actual == expected,
            "latest result differs from independent RPC; quiesce corpus and retry"
        );
    } else {
        for pattern in &case.patterns {
            let (actual, _) = runner.query(case, pattern, config.limit)?;
            let expected = reference_entries(config, case, pattern, &oracle)?;
            ensure!(
                actual == expected,
                "ordered result differs from independent RPC; quiesce corpus and retry"
            );
        }
    }
    Ok(references)
}

fn reference_entries(
    config: &Config,
    case: &Case,
    pattern: &str,
    oracle: &Client,
) -> Result<Vec<ResultEntry>> {
    if !config.reference.is_empty() {
        let query = match case.kind()? {
            scry_client::QueryKind::Prefix => scry_core::Query::Prefix(pattern.into()),
            scry_client::QueryKind::Substring => scry_core::Query::Substring(pattern.into()),
            scry_client::QueryKind::Wildcard => scry_core::Query::wildcard(pattern),
            scry_client::QueryKind::PathTerms => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)?
                    .as_secs()
                    .min(u32::MAX as u64) as u32;
                scry_core::terms::parse_query(pattern, now)
                    .map_err(|_| anyhow::anyhow!("invalid path terms"))?
            }
            _ => unreachable!(),
        };
        Ok(scenario::reference(
            &config.reference,
            &query,
            scry_core::view::SearchOptions::ordered(config.limit as usize, case.order()?),
        ))
    } else {
        oracle.query_ordered(case.kind()?, pattern, config.limit, case.order()?)
    }
}

fn sequence(
    config: &Config,
    case: &Case,
    runner: &mut Runner,
    record: bool,
    references: &[Reference],
) -> Result<Vec<serde_json::Value>> {
    let mut rows = Vec::new();
    if case.supersede {
        let Runner::Rpc(session) = runner else {
            unreachable!()
        };
        let started = Instant::now();
        for (step, pattern) in case.patterns.iter().enumerate() {
            if step > 0 {
                std::thread::sleep(Duration::from_millis(case.cadence_ms));
            }
            session.submit(case.kind()?, pattern)?;
        }
        let drain = Instant::now();
        let result = session.wait_latest()?;
        let drain_ns = drain.elapsed().as_nanos() as u64;
        let elapsed_ns = started.elapsed().as_nanos() as u64;
        ensure!(
            Reference::of(&result) == *references.last().unwrap(),
            "timed latest-wins answer differs from reference; capture rejected"
        );
        if record {
            let diagnostic = if config.cache_diagnostics {
                Some(refinement_diagnostics(&session.stats()?)?)
            } else {
                None
            };
            rows.push(
                json!({"step":case.patterns.len()-1,"execution":"rpc","elapsed_ns":elapsed_ns,
                "drain_ns":drain_ns,"emitted":result.len(),"requests":case.patterns.len(),"validated":true,"expected_emitted":references.last().unwrap().count,"answer_sha256":references.last().unwrap().hex(),"refinement":diagnostic}),
            );
        }
    } else {
        for (step, pattern) in case.patterns.iter().enumerate() {
            if step > 0 {
                std::thread::sleep(Duration::from_millis(case.cadence_ms));
            }
            let started = Instant::now();
            let (result, execution) = runner.query(case, pattern, config.limit)?;
            let elapsed_ns = started.elapsed().as_nanos() as u64;
            ensure!(
                Reference::of(&result) == references[step],
                "timed answer differs from reference at step {step}; capture rejected"
            );
            if record {
                let diagnostic = if config.cache_diagnostics {
                    match runner {
                        Runner::Rpc(session) => Some(refinement_diagnostics(&session.stats()?)?),
                        Runner::Local(_) => None,
                    }
                } else {
                    None
                };
                rows.push(json!({"step":step,"execution":execution,"elapsed_ns":elapsed_ns,"emitted":result.len(),"requests":1,"validated":true,"expected_emitted":references[step].count,"answer_sha256":references[step].hex(),"refinement":diagnostic}));
            }
        }
    }
    Ok(rows)
}

fn run(mut config: Config) -> Result<()> {
    let mut out = config.writer()?;
    let stats = config.connect()?;
    let initial_stats = stats.stats()?;
    if config.cache_diagnostics {
        refinement_diagnostics(&initial_stats)?;
    }
    let setting = |key: &str| {
        initial_stats
            .split_whitespace()
            .find_map(|token| token.strip_prefix(&format!("{key}=")))
    };
    let runtime = json!({"query_threads":setting("query_threads").and_then(|value|value.parse::<usize>().ok()),
        "refinement_cache":setting("refinement_cache").and_then(|value|value.parse::<bool>().ok()),
        "benchmark_frozen":setting("benchmark_frozen").and_then(|value|value.parse::<bool>().ok()),
        "affinity_mask":setting("affinity_mask").and_then(|value|value.parse::<u64>().ok())});
    if let Some(generation) = &config.workload.generation {
        ensure!(
            config.reference_sha256.as_deref() == Some(generation.snapshot_sha256.as_str()),
            "generated workload requires its exact reference snapshot"
        );
        ensure!(
            config.reference.len() == 1
                && config.reference[0].delta.added.is_empty()
                && config.reference[0].delta.tombstones.count_ones() == 0,
            "generated strata require one unmodified snapshot"
        );
        let reference = &config.reference[0];
        for (stratum, case) in generation.cases.iter().zip(&config.workload.cases) {
            ensure!(
                case.patterns.len() == 1
                    && generate::count(
                        &reference.base,
                        &case.kind,
                        &case.patterns[0],
                        config.output.parent().unwrap()
                    )? == stratum.matches,
                "generated workload selectivity changed"
            );
        }
    }
    if !config.reference.is_empty() {
        ensure!(
            runtime["benchmark_frozen"] == true,
            "--snapshot requires a daemon running in frozen benchmark mode"
        );
        let fingerprints: Vec<_> = config
            .reference
            .iter()
            .map(|reference| fingerprint::compute(&reference.base, config.output.parent().unwrap()))
            .collect::<Result<_>>()?;
        config.fingerprint = Some(if fingerprints.len() == 1 {
            fingerprints.into_iter().next().unwrap()
        } else {
            json!({"volumes":fingerprints})
        });
        config.workload.corpus.records =
            Some(config.reference.iter().map(|v| v.len() as u64).sum());
        config.workload.corpus.volumes = Some(config.reference.len() as u32);
        config.workload.corpus.cache_state =
            "OS cache uncontrolled; snapshot hashing and fingerprint scan precede timing".into();
        config.workload.corpus.delta_state = if config
            .reference
            .iter()
            .any(|v| !v.delta.added.is_empty() || v.delta.tombstones.count_ones() != 0)
        {
            "frozen static overlay; no ingestion"
        } else {
            "frozen daemon snapshot; empty delta; no ingestion"
        }
        .into();
    }
    write_row(
        &mut out,
        json!({"type":"metadata","schema":3,
        "unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
        "client_commit":env!("SCRY_BENCH_COMMIT"),"rustc":env!("SCRY_BENCH_RUSTC"),
        "client_tracked_dirty":env!("SCRY_BENCH_TRACKED_DIRTY"),
        "profile":env!("SCRY_BENCH_PROFILE"),"opt_level":env!("SCRY_BENCH_OPT_LEVEL"),
        "target":env!("SCRY_BENCH_TARGET"),"rustflags":env!("SCRY_BENCH_CARGO_ENCODED_RUSTFLAGS"),
        "daemon_build":config.daemon_build,"machine":config.machine,"power":config.power,
        "corpus":config.workload.corpus,"workload_hash":config.workload_hash,
        "samples":config.samples,"warmups":config.warmups,"rounds":config.rounds,"limit":config.limit,
        "fresh_process":true,"os_cache":"uncontrolled","background_connections":"operator controlled","client_affinity":observe::current_affinity()?,
        "verification":"preflight full-entry equality; every timed answer checked with ordered full-entry SHA-256 outside timers",
        "timed_validation":"ordered_full_entry_sha256", "reference_snapshot_sha256":config.reference_sha256,
        "corpus_fingerprint":config.fingerprint,"daemon_runtime":runtime,"generation":config.workload.generation,"cache_diagnostics":config.cache_diagnostics.then_some(true)}),
    )?;
    out.flush()?;

    // One dedicated stats connection remains idle during each timed batch.
    // There is no sampler thread or per-query memory syscall in this runner.
    for round in 0..config.rounds {
        for case in &config.workload.cases {
            for local in if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let requested = if local { "local_first" } else { "rpc" };
                if case.supersede && local {
                    write_row(
                        &mut out,
                        json!({"type":"skipped","round":round,"case":case.label,
                        "requested":requested,"reason":"local API is synchronous"}),
                    )?;
                    continue;
                }
                let references = verify(&config, case, local)?;
                write_row(
                    &mut out,
                    json!({"type":"verification","round":round,"case":case.label,
                    "requested":requested,"ordered_equal":true}),
                )?;
                let mut runner = Runner::new(&config, case, local)?;
                let before = stats.stats()?;
                for sample in 0..=config.samples {
                    let rows = sequence(&config, case, &mut runner, true, &references)?;
                    for mut row in rows {
                        let step = row["step"].as_u64().unwrap();
                        let phase = if sample == 0 && (step == 0 || case.supersede) {
                            "first_request"
                        } else if sample == 0 {
                            "first_sequence_later_step"
                        } else {
                            "warm"
                        };
                        let obj = row.as_object_mut().unwrap();
                        obj.extend(json!({"type":"sample","round":round,"case":case.label,"shape":case.shape,
                            "kind":case.kind,"order":case.order,"requested":requested,"phase":phase,
                            "sample":sample,"supersede":case.supersede,"cadence_ms":case.cadence_ms}).as_object().unwrap().clone());
                        write_row(&mut out, row)?;
                    }
                    if sample == 0 {
                        for _ in 0..config.warmups {
                            sequence(&config, case, &mut runner, false, &references)?;
                        }
                    }
                }
                let after = stats.stats()?;
                write_row(
                    &mut out,
                    json!({"type":"daemon_stats","round":round,"case":case.label,
                    "requested":requested,"before":before,"after":after,
                    "scope":"process-global; endpoint memory, not an interval peak"}),
                )?;
                out.flush()?;
                eprintln!(
                    "completed round {} case {} route {requested}",
                    round + 1,
                    case.label
                );
            }
        }
    }
    write_row(&mut out, json!({"type":"complete"}))?;
    out.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("generate-workload") => return generate::command(),
        Some("synthesize") => return synthetic::command(),
        Some("observe") => return observe::command(),
        Some("scenario") => return scenario::command(),
        _ => {}
    }
    if std::env::args().nth(1).as_deref() == Some("fingerprint") {
        return fingerprint::command();
    }
    if let Some(config) = Config::parse()? {
        // Client errors can contain pipe names. Keep raw errors out of logs.
        run(config).map_err(|error| anyhow::anyhow!("benchmark failed: {}", error.root_cause()))?;
    }
    Ok(())
}
