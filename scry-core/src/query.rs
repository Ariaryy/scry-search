use crate::arena::ArchivedArena;
use crate::cancel::{Cancellation, CHECK_INTERVAL};
use crate::literals::required_literals;
use crate::metrics::QuerySpans;
use regex_automata::meta::Regex;
use regex_automata::util::syntax;

mod parallel;
pub(crate) use parallel::bucket_shards;
use parallel::{scan_full_parallel, scan_full_parallel_ranked};

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
    /// Path terms plus metadata predicates applied before bounded top-k retention.
    FilteredPathTerms {
        terms: Vec<String>,
        filter: QueryFilter,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryFilter {
    pub kind: Option<EntryKind>,
    pub extensions: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub min_mtime: Option<u32>,
    pub max_mtime: Option<u32>,
}

impl QueryFilter {
    pub fn is_empty(&self) -> bool {
        self.kind.is_none()
            && self.extensions.is_empty()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.min_mtime.is_none()
            && self.max_mtime.is_none()
    }
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
            let cancelled =
                scan_substring_impl(arena, needle, cancel, spans, |idx, _name, _repeated| {
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
            let cancelled =
                scan_regex_impl(arena, pattern, cancel, spans, |idx, _name, _repeated| {
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
        Query::PathTerms(_) | Query::FilteredPathTerms { .. } => Vec::new(),
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
    mut sink: impl FnMut(u32, &[u8], bool) -> std::ops::ControlFlow<()>,
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
    let mut matched = false;
    let mut visit = |idx, name: &[u8], repeated: bool| {
        if is_cancelled_periodically(cancel, &mut checked) {
            cancelled.set(true);
            stopped.set(true);
            return std::ops::ControlFlow::Break(());
        }
        if !repeated {
            matched = finder.is_match(name);
        }
        if matched && sink(idx, name, repeated).is_break() {
            stopped.set(true);
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(())
    };
    match arena.candidate_blocks(&needle_lower) {
        None => {
            record_full_scan_blocks(arena, spans.as_deref_mut());
            arena.for_each_name_with_repeats(&mut visit)
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
                arena.for_each_name_in_with_repeats(start..end, &mut visit);
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
    mut sink: impl FnMut(u32, &[u8], bool) -> std::ops::ControlFlow<()>,
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
    let mut matched = false;
    let mut visit = |idx, name: &[u8], repeated: bool| {
        if is_cancelled_periodically(cancel, &mut checked) {
            cancelled.set(true);
            stopped.set(true);
            return std::ops::ControlFlow::Break(());
        }
        if !repeated {
            matched = re.is_match(name);
        }
        if matched && sink(idx, name, repeated).is_break() {
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
            arena.for_each_name_with_repeats(&mut visit)
        }
        Some(blocks) => {
            for block in blocks {
                let start = block * crate::trigram::TRIGRAM_BLOCK as u32;
                let end = (start + crate::trigram::TRIGRAM_BLOCK as u32).min(arena.len() as u32);
                arena.for_each_name_in_with_repeats(start..end, &mut visit);
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
/// The key callback is cloned per worker so name-derived ranking state stays
/// local. Repeat flags reset at each bucket/range head, and the callback still
/// runs for every matching record, including tombstones it may reject.
pub(crate) fn search_ranked_streaming(
    arena: &ArchivedArena,
    query: &Query,
    limit: usize,
    threads: usize,
    cancel: Option<Cancellation>,
    mut spans: Option<&mut QuerySpans>,
    mut key_for: impl FnMut(u32, &[u8], bool) -> Option<u64> + Clone + Send,
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
        let mut visit = |idx, name: &[u8], repeated| {
            if let Some(key) = key_for(idx, name, repeated) {
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
mod tests;
