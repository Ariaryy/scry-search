use super::*;

#[cfg(windows)]
#[test]
#[ignore = "release-mode query memory/latency probe; requires SCRY_BENCH_SNAPSHOT"]
fn benchmark_query_scratch_memory() {
    let path = std::env::var_os("SCRY_BENCH_SNAPSHOT")
        .expect("set SCRY_BENCH_SNAPSHOT to a synthetic benchmark snapshot");
    let view = IndexView::new(Arc::new(ArenaStore::open(Path::new(&path)).unwrap()));
    let query = Query::PathTerms(vec!["one".to_owned()]);
    query_probe::run("System", view.base.archived().len(), || {
        view.search(&query, 200)
    });
}

#[test]
#[ignore = "release-mode scratch capacity attribution; requires SCRY_BENCH_SNAPSHOT"]
fn benchmark_path_scratch_capacity() {
    let path = std::env::var_os("SCRY_BENCH_SNAPSHOT")
        .expect("set SCRY_BENCH_SNAPSHOT to a synthetic benchmark snapshot");
    let view = IndexView::new(Arc::new(ArenaStore::open(Path::new(&path)).unwrap()));
    for (label, terms) in [
        ("common", vec!["one".to_owned()]),
        ("multi", vec!["one".to_owned(), "report".to_owned()]),
        ("short", vec!["p".to_owned()]),
    ] {
        let query = Query::PathTerms(terms);
        let hits = view.search_hits(&query, SearchOptions::new(200));
        PATH_SEARCH_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            for (index, set) in scratch.term_sets.iter().enumerate() {
                println!("{label}: term={index} interval_bytes={}", set.allocated_bytes());
            }
            let fold = [scratch.fold_a.allocated_bytes(), scratch.fold_b.allocated_bytes()];
            println!("{label}: records={} hits={} fold_bytes={fold:?} heap_bytes={} own_match_bytes={} candidate_bytes={} name_bytes={}",
                view.base.archived().len(), hits.len(), scratch.heap.capacity() * std::mem::size_of::<u64>(),
                scratch.own_matches.capacity(), scratch.candidate_blocks.capacity() * std::mem::size_of::<u32>() + scratch.candidate_bitmap.capacity() + scratch.trigram_hashes.capacity() * std::mem::size_of::<usize>(), scratch.name.capacity());
        });
    }
}

/// Decision harness for the delta compaction threshold. Unlike
/// `delta_ratio_equivalence_and_latency`, this times selection only with
/// the production-sized result limit, so path materialization cannot hide
/// the linear overlay scan. It also times the base-sized compaction at the
/// three candidate thresholds. The output is diagnostic rather than a
/// machine-independent assertion.
#[test]
#[ignore = "release-mode delta threshold decision harness"]
fn delta_threshold_decision_gate() {
    const BASE: usize = 100_000;
    const SAMPLES: usize = 31;

    fn median_ns(mut run: impl FnMut()) -> u128 {
        let mut samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = std::time::Instant::now();
            run();
            samples.push(started.elapsed().as_nanos());
        }
        samples.sort_unstable();
        samples[SAMPLES / 2]
    }

    let (_base_dir, base) = base_view(BASE);
    let root = base.base.archived().prefix_range("C:").start;
    let substring = Query::Substring("zzq_never_present".into());
    let path_terms = Query::PathTerms(vec!["zzq_never_present".into()]);

    for basis_points in [0usize, 50, 100, 500, 1_000, 2_000] {
        let added = BASE * basis_points / 10_000;
        let mut delta = Delta::new(base.base.archived().len());
        delta.added.reserve(added);
        for index in 0..added {
            delta.added.push(DeltaRecord {
                name: format!("overlay_{index:06}.bin"),
                parent: ParentRef::Base(root),
                mtime_secs: 0,
                is_dir: false,
                size_bytes: 0,
                size_exact: true,
                live: true,
            });
        }
        let view = IndexView {
            base: Arc::clone(&base.base),
            delta: Arc::new(delta),
            generation: base.generation,
            journal_id: base.journal_id,
            next_usn: base.next_usn,
            volume_serial: base.volume_serial,
        };
        let options = SearchOptions::new(50);

        // Warm thread-local scratch and mapped pages before sampling.
        std::hint::black_box(view.search_hits(&substring, options));
        std::hint::black_box(view.search_hits(&path_terms, options));
        let substring_ns = median_ns(|| {
            std::hint::black_box(view.search_hits(&substring, options));
        });
        let path_terms_ns = median_ns(|| {
            std::hint::black_box(view.search_hits(&path_terms, options));
        });
        eprintln!(
            "delta {:>4.1}% ({added:>6} added): substring {:>8.1} us, path_terms {:>8.1} us",
            basis_points as f64 / 100.0,
            substring_ns as f64 / 1_000.0,
            path_terms_ns as f64 / 1_000.0,
        );

        if [50, 100, 500].contains(&basis_points) {
            let started = std::time::Instant::now();
            let (_dir, compacted) = open_compacted(&view);
            let elapsed = started.elapsed();
            std::hint::black_box(compacted.len());
            eprintln!(
                "delta {:>4.1}% compaction: {elapsed:?}",
                basis_points as f64 / 100.0,
            );
        }
    }
}

#[test]
#[ignore = "million-record release benchmark"]
fn benchmark_path_term_candidate_blocks() {
    const GROUPS: usize = 128;
    const FILES_PER_GROUP: usize = 8_192;
    const SAMPLES: usize = 9;

    let mut builder = crate::ArenaBuilder::with_capacity(
        GROUPS * FILES_PER_GROUP + GROUPS + 1,
        32 * GROUPS * FILES_PER_GROUP,
    );
    let root = builder.push("root", 0, true);
    for group in 0..GROUPS {
        let directory = builder.push(&format!("component_{group:03}_qzxv"), 0, true);
        builder.set_parent(directory, root);
        for file in 0..FILES_PER_GROUP {
            let record = builder.push(&format!("module_{group:03}_{file:06}_wkyp.dll"), 0, false);
            builder.set_parent(record, directory);
        }
    }

    let (arena, _) = builder.build();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("path-term-benchmark.rkyv");
    crate::store::save(&arena, &path).unwrap();
    let base = Arc::new(ArenaStore::open(&path).unwrap());
    let view = IndexView::new(base);
    let terms = vec![
        "component_073_qzxv".to_owned(),
        "073_000042_wkyp".to_owned(),
    ];

    let mut selected = Vec::new();
    for term in &terms {
        selected.extend(
            view.base
                .archived()
                .candidate_blocks(term.as_bytes())
                .unwrap(),
        );
    }
    selected.sort_unstable();
    selected.dedup();
    let total_blocks = view
        .base
        .archived()
        .len()
        .div_ceil(crate::trigram::TRIGRAM_BLOCK);

    let run = |filtered, scratch: &mut PathSearchScratch| {
        let start = std::time::Instant::now();
        let results = search_path_terms_with_scratch(
            view.base.archived(),
            &view.delta,
            &terms,
            &QueryFilter::default(),
            SearchOptions::new(50),
            scratch,
            filtered,
            None,
            1,
        );
        (start.elapsed(), results)
    };
    let (_, expected) = run(false, &mut PathSearchScratch::default());
    let (_, actual) = run(true, &mut PathSearchScratch::default());
    assert_eq!(actual, expected);

    let mut filtered_times = Vec::with_capacity(SAMPLES);
    let mut full_times = Vec::with_capacity(SAMPLES);
    let mut filtered_scratch = PathSearchScratch::default();
    let mut full_scratch = PathSearchScratch::default();
    for _ in 0..SAMPLES {
        filtered_times.push(run(true, &mut filtered_scratch).0);
        full_times.push(run(false, &mut full_scratch).0);
    }
    filtered_times.sort_unstable();
    full_times.sort_unstable();
    let median = SAMPLES / 2;
    eprintln!(
        "path terms: filtered={:?}, full={:?}, blocks={}/{total_blocks} ({:.2}%)",
        filtered_times[median],
        full_times[median],
        selected.len(),
        selected.len() as f64 * 100.0 / total_blocks as f64,
    );
}

#[test]
#[ignore = "release-mode short path-term phase probe"]
fn benchmark_short_path_term_phases() {
    const RECORDS: usize = 400_000;
    const SAMPLES: usize = 7;
    let mut builder = crate::ArenaBuilder::with_capacity(RECORDS + 1, RECORDS * 24);
    let root = builder.push("root", 0, true);
    for index in 0..RECORDS {
        let record = builder.push(&format!("archive_component_{index:06}.dat"), 0, false);
        builder.set_parent(record, root);
    }
    let (arena, _) = builder.build();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("short-path-term-phases.rkyv");
    crate::store::save(&arena, &path).unwrap();
    let view = IndexView::new(Arc::new(ArenaStore::open(&path).unwrap()));

    for (label, term) in [
        ("one_byte", "a"),
        ("two_byte", "ar"),
        ("three_byte", "arc"),
        ("rare", "component_399999"),
        ("absent", "not_present_qzxv"),
    ] {
        for threads in [1usize, 8] {
            let query = Query::PathTerms(vec![term.to_owned()]);
            let mut samples = Vec::with_capacity(SAMPLES);
            for _ in 0..SAMPLES {
                PATH_TERM_SCAN_NS.store(0, AtomicOrdering::Relaxed);
                PATH_TERM_COALESCE_NS.store(0, AtomicOrdering::Relaxed);
                PATH_TERM_ENUMERATE_NS.store(0, AtomicOrdering::Relaxed);
                let started = std::time::Instant::now();
                std::hint::black_box(view.search_hits_cancellable(
                    &query,
                    SearchOptions::new(50),
                    None,
                    threads,
                ));
                samples.push((
                    started.elapsed().as_nanos() as u64,
                    PATH_TERM_SCAN_NS.load(AtomicOrdering::Relaxed),
                    PATH_TERM_COALESCE_NS.load(AtomicOrdering::Relaxed),
                    PATH_TERM_ENUMERATE_NS.load(AtomicOrdering::Relaxed),
                ));
            }
            samples.sort_unstable_by_key(|sample| sample.0);
            let (total, scan, coalesce, enumerate) = samples[SAMPLES / 2];
            eprintln!(
                "{label} threads={threads}: total={:.3} ms scan={:.3} ms ({:.1}%) coalesce={:.3} ms enumerate={:.3} ms",
                total as f64 / 1e6,
                scan as f64 / 1e6,
                scan as f64 * 100.0 / total as f64,
                coalesce as f64 / 1e6,
                enumerate as f64 / 1e6,
            );
        }
    }
}

#[test]
#[ignore = "requires SCRY_LIVE_SNAPSHOT; prints short path-term phases"]
fn benchmark_live_short_path_term_phases() {
    const SAMPLES: usize = 9;
    let path = std::env::var_os("SCRY_LIVE_SNAPSHOT")
        .expect("set SCRY_LIVE_SNAPSHOT to an index snapshot");
    let view = IndexView::new(Arc::new(
        ArenaStore::open(std::path::Path::new(&path)).unwrap(),
    ));
    let query = Query::PathTerms(vec!["a".to_owned()]);
    for threads in [1usize, 8] {
        let mut samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            PATH_TERM_SCAN_NS.store(0, AtomicOrdering::Relaxed);
            PATH_TERM_COALESCE_NS.store(0, AtomicOrdering::Relaxed);
            PATH_TERM_ENUMERATE_NS.store(0, AtomicOrdering::Relaxed);
            let started = std::time::Instant::now();
            std::hint::black_box(view.search_hits_cancellable(
                &query,
                SearchOptions::new(200),
                None,
                threads,
            ));
            samples.push((
                started.elapsed().as_nanos() as u64,
                PATH_TERM_SCAN_NS.load(AtomicOrdering::Relaxed),
                PATH_TERM_COALESCE_NS.load(AtomicOrdering::Relaxed),
                PATH_TERM_ENUMERATE_NS.load(AtomicOrdering::Relaxed),
            ));
        }
        samples.sort_unstable_by_key(|sample| sample.0);
        let (total, scan, coalesce, enumerate) = samples[SAMPLES / 2];
        eprintln!(
            "records={} threads={threads}: total={:.3} ms scan={:.3} ms coalesce={:.3} ms enumerate={:.3} ms",
            view.base.archived().len(),
            total as f64 / 1e6,
            scan as f64 / 1e6,
            coalesce as f64 / 1e6,
            enumerate as f64 / 1e6,
        );
    }
}
