use super::{search_base, Query};
use crate::arena::{ArchivedArena, Arena};
use crate::store::{save, ArenaStore};
use regex_automata::meta::Regex;
use regex_automata::util::syntax;

fn generated_store(count: usize) -> (tempfile::TempDir, ArenaStore) {
    let mut builder = Arena::builder();
    for i in 0..count {
        let name = match i % 10 {
            0 => format!("Report_{i:06}.PDF"),
            1 => format!("summary_{i:06}.pdf"),
            2 => format!("image_{i:06}.png"),
            3 => format!("photo_{i:06}.jpeg"),
            4 => format!("a{i:06}b.c"),
            _ => format!("unrelated_{i:06}.dat"),
        };
        builder.push(&name, 0, false);
    }
    let arena = builder.build().0;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("regex-filter.rkyv");
    save(&arena, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    (dir, store)
}

fn regex_scan(arena: &ArchivedArena, pattern: &str, limit: usize) -> Vec<u32> {
    let re = Regex::builder()
        .syntax(syntax::Config::new().case_insensitive(true))
        .build(pattern)
        .unwrap();
    let mut results = Vec::new();
    arena.for_each_name(|idx, name| {
        if re.is_match(name) {
            results.push(idx);
            if results.len() >= limit {
                return std::ops::ControlFlow::Break(());
            }
        }
        std::ops::ControlFlow::Continue(())
    });
    results
}

#[test]
fn regex_results_are_identical_with_and_without_the_filter() {
    let (_dir, store) = generated_store(20_000);
    let arena = store.archived();
    let patterns = [
        r"^.*\.pdf$",
        r"^report.*$",
        r"^.*report.*$",
        r"^.*\.(png|jpeg)$",
        r"^a.b\.c$",
        r"^(ab|report).*$",
        r".*",
        r"^does-not-exist$",
    ];
    for pattern in patterns {
        for limit in [1, 7, usize::MAX] {
            assert_eq!(
                search_base(arena, &Query::Regex(pattern.into()), limit),
                regex_scan(arena, pattern, limit),
                "{pattern}, limit={limit}"
            );
        }
    }

    let clauses = crate::literals::required_literals(r"^.*\.jpeg$").unwrap();
    let selected = arena.candidate_blocks_for_clauses(&clauses).unwrap().len();
    let total = arena.len().div_ceil(crate::trigram::TRIGRAM_BLOCK);
    assert!(
        selected * 5 < total,
        "filter selected {selected}/{total} blocks"
    );
}

#[test]
fn wildcard_results_are_identical_with_and_without_the_filter() {
    let (_dir, store) = generated_store(20_000);
    let arena = store.archived();
    for wildcard in ["*.pdf", "report*", "*report*", "*.?ng", "*"] {
        let Query::Regex(pattern) = Query::wildcard(wildcard) else {
            unreachable!()
        };
        for limit in [1, 7, usize::MAX] {
            assert_eq!(
                search_base(arena, &Query::Regex(pattern.clone()), limit),
                regex_scan(arena, &pattern, limit),
                "{wildcard}, limit={limit}"
            );
        }
    }
}

/// Sharding the unfiltered scan across threads must never change which
/// records are found — only how the work to find them is split up. A
/// mismatch here would mean a query result depends on how many CPUs the
/// machine happens to have.
#[test]
fn parallel_scan_matches_sequential_scan() {
    let (_dir, store) = generated_store(5_000);
    let arena = store.archived();
    let substrings = ["a", "e", "0", "report", "xyz-not-present"];
    let regexes = [r".*", r"^a.*", r"^does-not-exist$"];
    for threads in [1, 2, 3, 8] {
        for pattern in substrings {
            let query = Query::Substring(pattern.to_string());
            let sequential = search_base(arena, &query, usize::MAX);
            let parallel = super::search_base_parallel(arena, &query, threads, None);
            let mut sequential_sorted = sequential.clone();
            let mut parallel_sorted = parallel.clone();
            sequential_sorted.sort_unstable();
            parallel_sorted.sort_unstable();
            assert_eq!(
                sequential_sorted, parallel_sorted,
                "substring {pattern:?}, threads={threads}"
            );
        }
        for pattern in regexes {
            let query = Query::Regex(pattern.to_string());
            let sequential = search_base(arena, &query, usize::MAX);
            let parallel = super::search_base_parallel(arena, &query, threads, None);
            let mut sequential_sorted = sequential.clone();
            let mut parallel_sorted = parallel.clone();
            sequential_sorted.sort_unstable();
            parallel_sorted.sort_unstable();
            assert_eq!(
                sequential_sorted, parallel_sorted,
                "regex {pattern:?}, threads={threads}"
            );
        }
    }
}

/// A cancelled parallel scan must discard everything it found, not just
/// whichever shard happened to notice — a superseded query still has to
/// come back empty, the same contract the sequential scan honors.
#[test]
fn parallel_scan_honors_cancellation() {
    let (_dir, store) = generated_store(200_000);
    let arena = store.archived();
    let generation = std::sync::atomic::AtomicU64::new(1);
    let cancel = crate::cancel::Cancellation::new(&generation, 0); // already stale
    let result =
        super::search_base_parallel(arena, &Query::Substring("a".to_string()), 4, Some(cancel));
    assert!(result.is_empty());
}

#[test]
fn repeated_negative_names_still_poll_cancellation() {
    let mut builder = crate::Arena::builder();
    let root = builder.push("C:", 0, true);
    let first = builder.push("alpha_marker", 0, false);
    builder.set_parent(first, root);
    for _ in 0..100_000 {
        let record = builder.push("z_unmatched", 0, false);
        builder.set_parent(record, root);
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("duplicate-cancel.rkyv");
    crate::store::save(&builder.build().0, &path).unwrap();
    let store = crate::store::ArenaStore::open(&path).unwrap();
    let generation = std::sync::atomic::AtomicU64::new(0);
    let mut emitted = 0;
    let cancelled = super::scan_substring_impl(
        store.archived(),
        "p",
        Some(crate::cancel::Cancellation::new(&generation, 0)),
        None,
        |_, _, _| {
            emitted += 1;
            generation.store(1, std::sync::atomic::Ordering::Relaxed);
            std::ops::ControlFlow::Continue(())
        },
    );
    assert!(cancelled);
    assert_eq!(emitted, 1);
}

#[test]
#[ignore = "million-record release benchmark"]
fn bench_regex_filtered_vs_scan() {
    let (_dir, store) = generated_store(1_000_000);
    let arena = store.archived();
    for pattern in [
        r"^.*\.pdf$",
        r"^report.*$",
        r"^.*report.*$",
        r"^.*\.(png|jpeg)$",
        r".*",
    ] {
        let start = std::time::Instant::now();
        let filtered = search_base(arena, &Query::Regex(pattern.into()), usize::MAX);
        let filtered_elapsed = start.elapsed();
        let start = std::time::Instant::now();
        let scanned = regex_scan(arena, pattern, usize::MAX);
        let scan_elapsed = start.elapsed();
        assert_eq!(filtered, scanned, "{pattern}");

        let selected = crate::literals::required_literals(pattern)
            .and_then(|clauses| arena.candidate_blocks_for_clauses(&clauses))
            .map(|blocks| blocks.len());
        eprintln!(
            "regex {pattern:?}: filtered={filtered_elapsed:?}, scan={scan_elapsed:?}, blocks={selected:?}/{}",
            arena.len().div_ceil(crate::trigram::TRIGRAM_BLOCK)
        );
    }
}
