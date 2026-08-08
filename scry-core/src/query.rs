use crate::arena::ArchivedArena;
use crate::cancel::{Cancellation, CHECK_INTERVAL};
use crate::literals::required_literals;
use crate::metrics::QuerySpans;
use regex_automata::meta::Regex;
use regex_automata::util::syntax;

#[derive(Debug, Clone)]
pub enum Query {
    /// Anchored prefix — binary search over name-sorted records, O(log n + k).
    Prefix(String),
    /// Unanchored substring, accelerated by a trigram block filter.
    Substring(String),
    /// Wildcard (`*`/`?`) or regex, compiled to a DFA once and reused across the scan.
    Regex(String),
    /// AND-ed literal terms that may match the leaf or any ancestor.
    PathTerms(Vec<String>),
}

impl Query {
    pub fn wildcard(pattern: &str) -> Self {
        let mut re = String::with_capacity(pattern.len() + 2);
        re.push('^');
        for c in pattern.chars() {
            match c {
                '*' => re.push_str(".*"),
                '?' => re.push('.'),
                '.' | '+' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\' => {
                    re.push('\\');
                    re.push(c);
                }
                c => re.push(c),
            }
        }
        re.push('$');
        Query::Regex(re)
    }
}

/// Search results are returned as arena indices — callers reconstruct paths
/// lazily via `ArchivedArena::full_path`, and only for the slice they actually
/// send over IPC (streaming, not materializing the whole result set).
pub fn search_base(arena: &ArchivedArena, query: &Query, limit: usize) -> Vec<u32> {
    search_base_impl(arena, query, limit, None, None)
}

/// As `search_base` with `limit = usize::MAX`, but for `Substring`
/// and `Regex` queries that would otherwise fall back to an unfiltered scan
/// (needle under 3 bytes, or no literal the trigram index can use), splits
/// that scan across up to `threads` scoped threads instead of running it on
/// the connection thread alone. Buckets are the shard unit: a front-coded
/// bucket decodes from nothing outside itself, so splitting on bucket
/// boundaries needs no synchronization between shards. Every other query
/// shape (an already-filtered scan, `Prefix`'s binary search, `PathTerms`)
/// gets no benefit from more threads and runs exactly as it does today.
pub fn search_base_parallel(
    arena: &ArchivedArena,
    query: &Query,
    threads: usize,
    cancel: Option<Cancellation>,
) -> Vec<u32> {
    search_base_parallel_with_spans(arena, query, threads, cancel, None)
}

/// As [`search_base_parallel`], while recording filter coverage when enabled.
pub fn search_base_parallel_with_spans(
    arena: &ArchivedArena,
    query: &Query,
    threads: usize,
    cancel: Option<Cancellation>,
    spans: Option<&mut QuerySpans>,
) -> Vec<u32> {
    if threads <= 1 {
        return search_base_impl(arena, query, usize::MAX, cancel, spans);
    }
    match query {
        Query::Substring(needle) => {
            let needle_lower: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
            if arena.candidate_blocks(&needle_lower).is_some() {
                return search_base_impl(arena, query, usize::MAX, cancel, spans);
            }
            record_full_scan_blocks(arena, spans);
            let finder = aho_corasick::AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([needle.as_bytes()])
                .expect("single-pattern Aho-Corasick automaton always builds");
            scan_full_parallel(arena, threads, cancel, move |name| finder.is_match(name))
        }
        Query::Regex(pattern) => {
            let re = match Regex::builder()
                .syntax(syntax::Config::new().case_insensitive(true))
                .build(pattern)
            {
                Ok(re) => re,
                Err(_) => return Vec::new(),
            };
            let filtered = required_literals(pattern)
                .and_then(|clauses| arena.candidate_blocks_for_clauses(&clauses))
                .is_some();
            if filtered {
                return search_base_impl(arena, query, usize::MAX, cancel, spans);
            }
            scan_full_parallel(arena, threads, cancel, move |name| re.is_match(name))
        }
        _ => search_base_impl(arena, query, usize::MAX, cancel, spans),
    }
}

/// Splits `[0, arena.len())` into up to `threads` bucket-aligned ranges, the
/// same shard unit `scan_full_parallel`/`scan_full_parallel_ranked` scan
/// independently: a front-coded bucket decodes from nothing outside itself,
/// so a shard boundary never needs synchronization with its neighbors.
fn bucket_shards(n: usize, threads: usize) -> impl Iterator<Item = (u32, u32)> {
    let num_buckets = n.div_ceil(crate::record::BUCKET_SIZE);
    let shard_buckets = num_buckets.div_ceil(threads.max(1)).max(1);
    (0..num_buckets)
        .step_by(shard_buckets)
        .map(move |start_bucket| {
            let end_bucket = (start_bucket + shard_buckets).min(num_buckets);
            let start = (start_bucket * crate::record::BUCKET_SIZE) as u32;
            let end = (end_bucket * crate::record::BUCKET_SIZE).min(n) as u32;
            (start, end)
        })
}

/// Runs `matches` over every name in `arena`, sharded across `threads`
/// bucket-aligned ranges. On cancellation the merged result is discarded
/// entirely (returns empty), matching the single-threaded scan's contract
/// that a superseded query yields no results rather than a partial set.
fn scan_full_parallel(
    arena: &ArchivedArena,
    threads: usize,
    cancel: Option<Cancellation>,
    matches: impl Fn(&[u8]) -> bool + Sync,
) -> Vec<u32> {
    let n = arena.len();
    if n == 0 {
        return Vec::new();
    }
    let matches = &matches;
    let shards: Vec<Vec<u32>> = std::thread::scope(|scope| {
        bucket_shards(n, threads)
            .map(|(start, end)| {
                scope.spawn(move || {
                    let mut local = Vec::new();
                    let mut checked: u32 = 0;
                    arena.for_each_name_in(start..end, |idx, name| {
                        if is_cancelled_periodically(cancel, &mut checked) {
                            return std::ops::ControlFlow::Break(());
                        }
                        if matches(name) {
                            local.push(idx);
                        }
                        std::ops::ControlFlow::Continue(())
                    });
                    local
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    if cancel.is_some_and(|c| c.is_cancelled()) {
        return Vec::new();
    }
    shards.into_iter().flatten().collect()
}

/// As [`scan_full_parallel`], but ranks while it scans: each shard keeps only
/// its own top-`limit` in a bounded heap via [`rank::retain_hit`], and the
/// per-shard heaps are merged the same way. That merge is exact — any record
/// in the global top-`limit` must also be in its own shard's top-`limit`,
/// since a shard with `limit` or more strictly better matches would itself
/// disqualify it — so nothing outside the per-shard heaps needs to be kept.
/// Returns the merged heap and the total number of candidates seen (for
/// instrumentation), or `(empty, 0)` on cancellation.
fn scan_full_parallel_ranked(
    arena: &ArchivedArena,
    threads: usize,
    cancel: Option<Cancellation>,
    limit: usize,
    is_match: impl Fn(&[u8]) -> bool + Sync,
    key_for: impl Fn(u32, &[u8]) -> Option<u64> + Sync,
) -> (std::collections::BinaryHeap<u64>, u64) {
    let n = arena.len();
    if n == 0 || limit == 0 {
        return (std::collections::BinaryHeap::new(), 0);
    }
    let is_match = &is_match;
    let key_for = &key_for;
    let shards: Vec<(std::collections::BinaryHeap<u64>, u64)> = std::thread::scope(|scope| {
        bucket_shards(n, threads)
            .map(|(start, end)| {
                scope.spawn(move || {
                    let mut heap = std::collections::BinaryHeap::new();
                    let mut seen: u64 = 0;
                    let mut checked: u32 = 0;
                    arena.for_each_name_in(start..end, |idx, name| {
                        if is_cancelled_periodically(cancel, &mut checked) {
                            return std::ops::ControlFlow::Break(());
                        }
                        if is_match(name) {
                            if let Some(key) = key_for(idx, name) {
                                seen += 1;
                                crate::rank::retain_hit(&mut heap, key, limit);
                            }
                        }
                        std::ops::ControlFlow::Continue(())
                    });
                    (heap, seen)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    if cancel.is_some_and(|c| c.is_cancelled()) {
        return (std::collections::BinaryHeap::new(), 0);
    }
    let mut merged = std::collections::BinaryHeap::with_capacity(limit.min(4096));
    let mut total = 0u64;
    for (heap, seen) in shards {
        total += seen;
        for key in heap {
            crate::rank::retain_hit(&mut merged, key, limit);
        }
    }
    (merged, total)
}

fn search_base_impl(
    arena: &ArchivedArena,
    query: &Query,
    limit: usize,
    cancel: Option<Cancellation>,
    spans: Option<&mut QuerySpans>,
) -> Vec<u32> {
    match query {
        Query::Prefix(prefix) => {
            let range = arena.prefix_range(prefix);
            range.take(limit).collect()
        }
        Query::Substring(needle) => {
            let mut results = Vec::new();
            let cancelled = scan_substring_impl(arena, needle, cancel, spans, |idx, _name| {
                results.push(idx);
                if results.len() >= limit {
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            });
            if cancelled {
                return Vec::new();
            }
            results
        }
        Query::Regex(pattern) => {
            let mut results = Vec::new();
            let cancelled = scan_regex_impl(arena, pattern, cancel, spans, |idx, _name| {
                results.push(idx);
                if results.len() >= limit {
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            });
            if cancelled {
                return Vec::new();
            }
            results
        }
        Query::PathTerms(_) => Vec::new(),
    }
}

/// Runs `sink` over every name in `arena` matching `needle`
/// (ASCII-case-insensitive substring), using the trigram block filter when
/// the needle is long enough to have one and falling back to a full scan
/// otherwise. `sink` decides whether to keep scanning by its return value —
/// the count-limited caller in `search_base_impl` breaks once it has enough
/// unordered matches, while a ranked caller keeps going forever since a
/// later candidate can still outrank the current worst-in-heap. Shared so
/// both callers see exactly the same set of candidates in exactly the same
/// order. Returns whether the scan was cancelled (in which case the caller
/// must discard whatever `sink` already saw).
fn scan_substring_impl(
    arena: &ArchivedArena,
    needle: &str,
    cancel: Option<Cancellation>,
    mut spans: Option<&mut QuerySpans>,
    mut sink: impl FnMut(u32, &[u8]) -> std::ops::ControlFlow<()>,
) -> bool {
    // `needle_lower` drives the trigram block filter (the trigram index is
    // built lowercase); per-name matching runs directly over the raw bytes
    // via an ASCII-case-insensitive Aho-Corasick automaton, so no per-name
    // lowercase copy is made.
    let needle_lower: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let finder = aho_corasick::AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build([needle.as_bytes()])
        .expect("single-pattern Aho-Corasick automaton always builds");
    let stopped = std::cell::Cell::new(false);
    let cancelled = std::cell::Cell::new(false);
    let mut checked: u32 = 0;
    let mut visit = |idx, name: &[u8]| {
        if is_cancelled_periodically(cancel, &mut checked) {
            cancelled.set(true);
            stopped.set(true);
            return std::ops::ControlFlow::Break(());
        }
        if finder.is_match(name) && sink(idx, name).is_break() {
            stopped.set(true);
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(())
    };
    match arena.candidate_blocks(&needle_lower) {
        None => {
            record_full_scan_blocks(arena, spans.as_deref_mut());
            arena.for_each_name(&mut visit)
        }
        Some(blocks) => {
            if let Some(spans) = spans.as_mut() {
                spans.blocks_total = spans
                    .blocks_total
                    .saturating_add(arena.len().div_ceil(crate::trigram::TRIGRAM_BLOCK) as u64);
                spans.blocks_scanned = spans.blocks_scanned.saturating_add(blocks.len() as u64);
            }
            for block in blocks {
                let start = block * crate::trigram::TRIGRAM_BLOCK as u32;
                let end = (start + crate::trigram::TRIGRAM_BLOCK as u32).min(arena.len() as u32);
                arena.for_each_name_in(start..end, &mut visit);
                if stopped.get() {
                    break;
                }
            }
        }
    }
    cancelled.get()
}

/// As [`scan_substring_impl`], for a compiled regex/wildcard pattern.
fn scan_regex_impl(
    arena: &ArchivedArena,
    pattern: &str,
    cancel: Option<Cancellation>,
    spans: Option<&mut QuerySpans>,
    mut sink: impl FnMut(u32, &[u8]) -> std::ops::ControlFlow<()>,
) -> bool {
    let re = match Regex::builder()
        .syntax(syntax::Config::new().case_insensitive(true))
        .build(pattern)
    {
        Ok(re) => re,
        Err(_) => return false,
    };
    let stopped = std::cell::Cell::new(false);
    let cancelled = std::cell::Cell::new(false);
    let mut checked: u32 = 0;
    let mut visit = |idx, name: &[u8]| {
        if is_cancelled_periodically(cancel, &mut checked) {
            cancelled.set(true);
            stopped.set(true);
            return std::ops::ControlFlow::Break(());
        }
        if re.is_match(name) && sink(idx, name).is_break() {
            stopped.set(true);
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(())
    };
    match required_literals(pattern)
        .and_then(|clauses| arena.candidate_blocks_for_clauses(&clauses))
    {
        None => {
            record_full_scan_blocks(arena, spans);
            arena.for_each_name(&mut visit)
        }
        Some(blocks) => {
            for block in blocks {
                let start = block * crate::trigram::TRIGRAM_BLOCK as u32;
                let end = (start + crate::trigram::TRIGRAM_BLOCK as u32).min(arena.len() as u32);
                arena.for_each_name_in(start..end, &mut visit);
                if stopped.get() {
                    break;
                }
            }
        }
    }
    cancelled.get()
}

/// As [`search_base`], but ranks while it scans instead of collecting every
/// match into a `Vec<u32>` first and ranking it afterward — the caller
/// supplies `key_for` (built from the ordering, the arena, and the delta) and
/// gets back the winners as a bounded max-heap of sort keys, never seeing an
/// intermediate collection sized to the candidate count rather than `limit`.
/// Only `Query::Substring` and `Query::Regex` reach this: those are the
/// kinds whose scan is unbounded and worth ranking as it goes; `Prefix`
/// already resolves through a tight binary-search range and `PathTerms` is
/// handled elsewhere. Returns the merged heap and the number of candidates
/// seen, for `QuerySpans::candidates`.
pub(crate) fn search_ranked_streaming(
    arena: &ArchivedArena,
    query: &Query,
    limit: usize,
    threads: usize,
    cancel: Option<Cancellation>,
    mut spans: Option<&mut QuerySpans>,
    key_for: impl Fn(u32, &[u8]) -> Option<u64> + Sync,
) -> (std::collections::BinaryHeap<u64>, u64) {
    if limit == 0 {
        return (std::collections::BinaryHeap::new(), 0);
    }
    let filtered = match query {
        Query::Substring(needle) => {
            let needle_lower: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
            arena.candidate_blocks(&needle_lower).is_some()
        }
        Query::Regex(pattern) => required_literals(pattern)
            .and_then(|clauses| arena.candidate_blocks_for_clauses(&clauses))
            .is_some(),
        _ => unreachable!("search_ranked_streaming only handles Substring/Regex"),
    };
    if threads <= 1 || filtered {
        let mut heap = std::collections::BinaryHeap::with_capacity(limit.min(4096));
        let mut seen: u64 = 0;
        let mut visit = |idx, name: &[u8]| {
            if let Some(key) = key_for(idx, name) {
                seen += 1;
                crate::rank::retain_hit(&mut heap, key, limit);
            }
            std::ops::ControlFlow::Continue(())
        };
        let cancelled = match query {
            Query::Substring(needle) => {
                scan_substring_impl(arena, needle, cancel, spans.as_deref_mut(), &mut visit)
            }
            Query::Regex(pattern) => {
                scan_regex_impl(arena, pattern, cancel, spans.as_deref_mut(), &mut visit)
            }
            _ => unreachable!(),
        };
        if cancelled {
            return (std::collections::BinaryHeap::new(), 0);
        }
        (heap, seen)
    } else {
        record_full_scan_blocks(arena, spans);
        match query {
            Query::Substring(needle) => {
                let finder = aho_corasick::AhoCorasick::builder()
                    .ascii_case_insensitive(true)
                    .build([needle.as_bytes()])
                    .expect("single-pattern Aho-Corasick automaton always builds");
                scan_full_parallel_ranked(
                    arena,
                    threads,
                    cancel,
                    limit,
                    move |name| finder.is_match(name),
                    key_for,
                )
            }
            Query::Regex(pattern) => {
                let re = match Regex::builder()
                    .syntax(syntax::Config::new().case_insensitive(true))
                    .build(pattern)
                {
                    Ok(re) => re,
                    Err(_) => return (std::collections::BinaryHeap::new(), 0),
                };
                scan_full_parallel_ranked(
                    arena,
                    threads,
                    cancel,
                    limit,
                    move |name| re.is_match(name),
                    key_for,
                )
            }
            _ => unreachable!(),
        }
    }
}

fn record_full_scan_blocks(arena: &ArchivedArena, spans: Option<&mut QuerySpans>) {
    if let Some(spans) = spans {
        let total = arena.len().div_ceil(crate::trigram::TRIGRAM_BLOCK) as u64;
        spans.blocks_total = spans.blocks_total.saturating_add(total);
        spans.blocks_scanned = spans.blocks_scanned.saturating_add(total);
    }
}

/// Advances `checked` and, once every `CHECK_INTERVAL` calls, reports whether
/// `cancel` has been superseded. Cheap in the common (non-cancellable) case:
/// `cancel` is `None` for every one-shot caller, so this is a single branch.
#[inline]
pub(crate) fn is_cancelled_periodically(cancel: Option<Cancellation>, checked: &mut u32) -> bool {
    let Some(cancel) = cancel else {
        return false;
    };
    *checked = checked.wrapping_add(1);
    (*checked).is_multiple_of(CHECK_INTERVAL) && cancel.is_cancelled()
}

#[cfg(test)]
mod tests {
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
}
