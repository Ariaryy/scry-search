use super::*;

#[test]
#[ignore = "release-mode mimalloc query memory/latency probe; requires SCRY_BENCH_SNAPSHOT"]
fn benchmark_query_scratch_memory() {
    let path = std::env::var_os("SCRY_BENCH_SNAPSHOT")
        .expect("set SCRY_BENCH_SNAPSHOT to a synthetic benchmark snapshot");
    let view = IndexView::new(Arc::new(
        scry_core::store::ArenaStore::open(std::path::Path::new(&path)).unwrap(),
    ));
    let query = Query::PathTerms(vec!["one".to_owned()]);
    query_probe::run("mimalloc", view.base.archived().len(), || {
        view.search(&query, 200)
    });
}

/// Attributable plan-017 wall-time probe. The first keystroke primes the
/// per-connection state; only the second, 20,000-candidate overscan is
/// timed. The same function can be applied to the pre-change parent for a
/// direct before/after comparison.
#[test]
#[ignore = "release-mode bounded-refinement benchmark"]
fn benchmark_bounded_refinement_wall() {
    const ITERATIONS: usize = 60;
    let dir = tempfile::tempdir().unwrap();
    let view = build_store_with_n_records(30_000, &dir);
    let indexes = single_volume(view);
    let generation = std::sync::atomic::AtomicU64::new(0);
    let mut samples = Vec::with_capacity(ITERATIONS);
    let mut merge_samples = Vec::with_capacity(ITERATIONS);

    for _ in 0..ITERATIONS {
        let mut cache = RefinementCache::default();
        let _ = search_indexes_with_cache(
            &indexes,
            QueryKind::Substring,
            &Query::Substring("f".to_string()),
            SearchOptions::new(50),
            scry_core::Cancellation::new(&generation, 0),
            &mut cache,
        );
        let started = std::time::Instant::now();
        let mut spans = QuerySpans::default();
        let results = search_indexes_with_cache_with_spans(
            &indexes,
            QueryKind::Substring,
            &Query::Substring("fi".to_string()),
            SearchOptions::new(50),
            scry_core::Cancellation::new(&generation, 0),
            &mut cache,
            &mut spans,
        );
        samples.push(started.elapsed());
        merge_samples.push(std::time::Duration::from_nanos(spans.merge_ns));
        assert_eq!(results.len(), 50);
    }

    samples.sort_unstable();
    merge_samples.sort_unstable();
    println!(
        "bounded refinement: n={} p50={:?} p99={:?}; merge p50={:?} p99={:?}",
        samples.len(),
        samples[samples.len() / 2],
        samples[samples.len() * 99 / 100],
        merge_samples[merge_samples.len() / 2],
        merge_samples[merge_samples.len() * 99 / 100],
    );
}

/// Not a pass/fail test: exercises the daemon's real query path (cache,
/// span accumulation, memory sampling) end to end over a synthetic
/// corpus and prints a report, so the numbers in
/// `docs/internal/query-latency-baseline.md` come from this code path rather than
/// from a description of it. Run with
/// `cargo test -p scry-daemon --release -- --ignored span_report --nocapture`.
#[test]
#[ignore = "prints a report; not a pass/fail gate"]
fn span_report_over_a_synthetic_corpus() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = Arena::builder();
    let root = b.push("C:", 0, true);
    let teamdir = b.push("TEAMDIR", 0, true);
    b.set_parent(teamdir, root);
    let dirs = ["Documents", "Photos", "Projects", "Downloads"];
    let vocab = [
        "ledger", "report", "invoice", "photo", "backup", "notes", "draft", "project", "budget",
        "sketch", "summary", "ticket",
    ];
    let exts = ["txt", "pdf", "png", "docx", "log", "dll"];
    let mut i: u32 = 0;
    for &parent in &[teamdir, root] {
        for &d in &dirs {
            let dnode = b.push(&format!("{d}_{parent}"), 0, true);
            b.set_parent(dnode, parent);
            for &w in &vocab {
                for &ext in &exts {
                    for _ in 0..12 {
                        let name = format!("{w}_{i}.{ext}");
                        let f = b.push(&name, 1_700_000_000, false);
                        b.set_parent(f, dnode);
                        i += 1;
                    }
                }
            }
        }
    }
    let arena = b.build().0;
    let path = dir.path().join("report.rkyv");
    save(&arena, &path).unwrap();
    let view = Arc::new(IndexView::new(Arc::new(ArenaStore::open(&path).unwrap())));
    println!("\ncorpus: {i} records");
    let indexes = single_volume(view);
    let generation = std::sync::atomic::AtomicU64::new(0);

    let report_one = |label: &str, cache: &mut RefinementCache, kind, query: &Query| {
        let mut spans = QuerySpans::default();
        let entries = search_indexes_with_cache_with_spans(
            &indexes,
            kind,
            query,
            SearchOptions::new(50),
            scry_core::Cancellation::new(&generation, 0),
            cache,
            &mut spans,
        );
        println!(
                "  {label:22} hits={:<6} select_ns={:<9} finalize_ns={:<8} merge_ns={:<8} materialize_ns={:<8} \
                 candidates={:<7} emitted={}",
                entries.len(),
                spans.select_ns,
                spans.finalize_ns,
                spans.merge_ns,
                spans.materialize_ns,
                spans.candidates,
                spans.emitted,
            );
    };

    println!("\nmemory, cold:");
    println!("  {:?}", process_memory());

    println!("\nkeystroke sequence \"ledger\" (first query, then each refinement):");
    let mut cache = RefinementCache::default();
    let word = "ledger";
    for step in 1..=word.len() {
        let pattern = word[..step].to_string();
        let label = if step == 1 {
            "first (\"l\")".to_string()
        } else {
            pattern.clone()
        };
        report_one(
            &label,
            &mut cache,
            QueryKind::Substring,
            &Query::Substring(pattern),
        );
    }

    println!("\ncold query \".pdf\" (fresh connection):");
    let mut cache = RefinementCache::default();
    report_one(
        ".pdf",
        &mut cache,
        QueryKind::Substring,
        &Query::Substring("pdf".to_string()),
    );

    println!("\ncold query \"TEAMDIR ledger\" (PathTerms, fresh connection):");
    let mut cache = RefinementCache::default();
    report_one(
        "TEAMDIR ledger",
        &mut cache,
        QueryKind::PathTerms,
        &Query::PathTerms(vec!["TEAMDIR".to_string(), "ledger".to_string()]),
    );

    println!("\nmemory, warm (after the above):");
    println!("  {:?}", process_memory());
}

/// Real-volume diagnostic for Part E's STOP gate. Run elevated with
/// `cargo test -p scry-daemon --release -- --ignored --nocapture
/// query_p99_during_full_index_with_and_without_background_mode`.
/// Generic vocabulary keeps the output safe to retain in measurement
/// notes. This is ignored because it enumerates C: twice.
#[test]
#[ignore = "elevated real-volume contention benchmark"]
fn query_p99_during_full_index_with_and_without_background_mode() {
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    struct Sample {
        p50: std::time::Duration,
        p99: std::time::Duration,
        queries: usize,
        reindex_wall: std::time::Duration,
        records: usize,
    }

    fn run(view: &Arc<IndexView>, background: bool) -> Sample {
        trim_working_set();
        let query = Query::Substring("a".to_string());
        // Give both halves the same warm mapped-index starting point, so
        // the second run cannot win merely because the first faulted the
        // query columns into the system cache.
        for _ in 0..3 {
            let _ = view.search_hits(&query, SearchOptions::new(50));
        }
        let started = Arc::new(std::sync::Barrier::new(2));
        let done = Arc::new(AtomicBool::new(false));
        let worker_started = started.clone();
        let worker_done = done.clone();
        let worker = std::thread::spawn(move || {
            let _background = background.then(BackgroundModeGuard::enter);
            worker_started.wait();
            let began = std::time::Instant::now();
            let (arena, _) = scry_fsevents::WindowsBackend::bulk_index_volume("C:")
                .expect("elevated C: enumeration failed");
            let result = (began.elapsed(), arena.len());
            drop(arena);
            worker_done.store(true, AtomicOrdering::Release);
            result
        });

        started.wait();
        let mut latencies = Vec::new();
        while !done.load(AtomicOrdering::Acquire) {
            let began = std::time::Instant::now();
            let _ = view.search_hits(&query, SearchOptions::new(50));
            latencies.push(began.elapsed());
        }
        let (reindex_wall, records) = worker.join().unwrap();
        assert!(
            latencies.len() >= 20,
            "reindex ended before a useful sample"
        );
        latencies.sort_unstable();
        Sample {
            p50: latencies[latencies.len() / 2],
            p99: latencies[latencies.len() * 99 / 100],
            queries: latencies.len(),
            reindex_wall,
            records,
        }
    }

    scry_fsevents::configure_index_read_cap(128 * 1024 * 1024);
    let view = Arc::new(IndexView::new(Arc::new(
        ArenaStore::open(&snapshot_path("C:")).expect("C: snapshot is required"),
    )));
    let normal = run(&view, false);
    let background = run(&view, true);
    println!(
        "foreground-priority reindex: p50={:?} p99={:?} queries={} wall={:?} records={}",
        normal.p50, normal.p99, normal.queries, normal.reindex_wall, normal.records
    );
    println!(
        "background-priority reindex: p50={:?} p99={:?} queries={} wall={:?} records={}",
        background.p50,
        background.p99,
        background.queries,
        background.reindex_wall,
        background.records
    );
}
