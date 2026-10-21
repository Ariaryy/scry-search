//! Record metadata, filtering, and bounded top-k selection.

use std::collections::BinaryHeap;

use regex_automata::{meta::Regex, util::syntax};

use super::{Hit, SearchOptions};
use crate::ascii;
use crate::cancel::Cancellation;
use crate::delta::Delta;
use crate::metrics::QuerySpans;
use crate::query::{EntryKind, Query, QueryFilter};
use crate::rank::{self, Order};

/// Modification time of a record in the combined base-then-delta space.
#[inline]
fn record_mtime(arena: &crate::ArchivedArena, delta: &Delta, record: u32) -> u32 {
    match record.checked_sub(arena.len() as u32) {
        None => arena.mtime(record),
        Some(index) => delta.added[index as usize].mtime_secs,
    }
}

/// Size of a record in KiB — the width the index stores and the width
/// [`rank::largest_key`] wants. For a base-arena directory this is its
/// *recursive* size (everything beneath it, O(1) via the DFS prefix-sum
/// column); for a file, or for any delta-added record, it is the record's
/// own size — a delta addition has no subtree computed for it, and a file's
/// subtree is itself anyway.
#[inline]
fn record_size_kib(arena: &crate::ArchivedArena, delta: &Delta, record: u32) -> u32 {
    match record.checked_sub(arena.len() as u32) {
        None => arena.recursive_size_kib(record),
        Some(index) => crate::record::bytes_to_size_kib(delta.added[index as usize].size_bytes),
    }
}

#[inline]
fn record_size_exact(arena: &crate::ArchivedArena, delta: &Delta, record: u32) -> bool {
    match record.checked_sub(arena.len() as u32) {
        None => arena.size_exact(record),
        Some(index) => delta.added[index as usize].size_exact,
    }
}

/// The sort key for one candidate under `order`.
///
/// `quality` and `name_len` are already in hand at every call site; the cold
/// `mtimes`/`sizes` reads happen only for the orderings that need them, which
/// is the point of [`Order::needs_metadata`].
#[inline]
pub(super) fn sort_key(
    order: Order,
    arena: &crate::ArchivedArena,
    delta: &Delta,
    record: u32,
    quality: u8,
    name_len: u32,
) -> u64 {
    match order {
        Order::Relevance => rank::relevance_key(quality, name_len, record),
        Order::Recent => rank::recent_key(record_mtime(arena, delta, record), record),
        Order::Largest => rank::largest_key(
            if record_size_exact(arena, delta, record) {
                record_size_kib(arena, delta, record)
            } else {
                0
            },
            record,
        ),
    }
}

/// Turn the heap's retained keys into hits, best first.
pub(super) fn drain_heap(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    heap: &mut BinaryHeap<u64>,
) -> Vec<Hit> {
    let mut hits = Vec::with_capacity(heap.len());
    while let Some(key) = heap.pop() {
        hits.push(hit_for(
            arena,
            delta,
            rank::key_record(key),
            rank::key_rank_bits(key),
        ));
    }
    // The heap yields worst-first; the caller wants best-first.
    hits.reverse();
    hits
}

fn hit_for(arena: &crate::ArchivedArena, delta: &Delta, record: u32, rank_bits: u32) -> Hit {
    match record.checked_sub(arena.len() as u32) {
        None => Hit {
            record,
            rank_bits,
            size: if arena.is_dir(record) {
                arena.recursive_size_kib(record) as u64 * 1024
            } else {
                arena.size_bytes(record)
            },
            mtime: arena.mtime(record),
            is_dir: arena.is_dir(record),
            size_exact: arena.size_exact(record),
        },
        Some(index) => {
            let added = &delta.added[index as usize];
            Hit {
                record,
                rank_bits,
                size: added.size_bytes,
                mtime: added.mtime_secs,
                is_dir: added.is_dir,
                size_exact: added.size_exact,
            }
        }
    }
}

pub(super) fn match_quality(query: &Query, name: &[u8]) -> u8 {
    let pattern = match query {
        Query::Prefix(pattern) | Query::Substring(pattern) => pattern.as_bytes(),
        Query::Regex(_) => return 2,
        Query::PathTerms(_) | Query::FilteredPathTerms { .. } => unreachable!(),
    };
    if ascii::cmp_ci(name, pattern).is_eq() {
        0
    } else if ascii::starts_with_ci(name, pattern) {
        1
    } else {
        2
    }
}

pub(super) fn search_ranked(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    query: &Query,
    options: SearchOptions,
) -> Vec<Hit> {
    search_ranked_cancellable(arena, delta, query, options, None, 1)
}

fn search_ranked_cancellable(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    query: &Query,
    options: SearchOptions,
    cancel: Option<Cancellation>,
    threads: usize,
) -> Vec<Hit> {
    search_ranked_cancellable_with_spans(arena, delta, query, options, cancel, threads, None)
}

pub(super) fn search_ranked_cancellable_with_spans(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    query: &Query,
    options: SearchOptions,
    cancel: Option<Cancellation>,
    threads: usize,
    mut spans: Option<&mut QuerySpans>,
) -> Vec<Hit> {
    let SearchOptions { limit, order } = options;
    if limit == 0 {
        return Vec::new();
    }
    if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
        return Vec::new();
    }
    // `match_quality` needs the decoded name, and so does the length; neither
    // is needed by an ordering that ranks on a column, so skip the decode.
    let needs_name = !order.needs_metadata();
    let mut heap = match query {
        Query::Substring(_) | Query::Regex(_) => {
            let select_started = spans.as_ref().map(|_| std::time::Instant::now());
            let mut name_rank = None;
            let (heap, seen) = crate::query::search_ranked_streaming(
                arena,
                query,
                limit,
                threads,
                cancel,
                spans.as_deref_mut(),
                move |index, name, repeated| {
                    if !repeated {
                        name_rank = None;
                    }
                    if delta.tombstones.get(index) {
                        return None;
                    }
                    let (quality, name_len) = if needs_name {
                        *name_rank
                            .get_or_insert_with(|| (match_quality(query, name), name.len() as u32))
                    } else {
                        (0, 0)
                    };
                    Some(sort_key(order, arena, delta, index, quality, name_len))
                },
            );
            if let (Some(spans), Some(started)) = (spans.as_deref_mut(), select_started) {
                spans.select_ns = spans
                    .select_ns
                    .saturating_add(started.elapsed().as_nanos() as u64);
                spans.candidates = spans.candidates.saturating_add(seen);
            }
            heap
        }
        _ => {
            let select_started = spans.as_ref().map(|_| std::time::Instant::now());
            let base_hits = crate::query::search_base_parallel_with_spans(
                arena,
                query,
                threads,
                cancel,
                spans.as_deref_mut(),
            );
            if let Some(spans) = spans.as_deref_mut() {
                spans.candidates = spans.candidates.saturating_add(base_hits.len() as u64);
            }
            let mut heap = BinaryHeap::with_capacity(limit.min(4096));
            let mut name = Vec::new();
            for index in base_hits {
                if delta.tombstones.get(index) {
                    continue;
                }
                let (quality, name_len) = if needs_name {
                    arena.name_into(index, &mut name);
                    (match_quality(query, &name), name.len() as u32)
                } else {
                    (0, 0)
                };
                rank::retain_hit(
                    &mut heap,
                    sort_key(order, arena, delta, index, quality, name_len),
                    limit,
                );
            }
            if let (Some(spans), Some(started)) = (spans.as_deref_mut(), select_started) {
                spans.select_ns = spans
                    .select_ns
                    .saturating_add(started.elapsed().as_nanos() as u64);
            }
            heap
        }
    };
    let finalize_started = spans.as_ref().map(|_| std::time::Instant::now());

    let regex = match query {
        Query::Regex(pattern) => Regex::builder()
            .syntax(syntax::Config::new().case_insensitive(true))
            .build(pattern)
            .ok(),
        _ => None,
    };
    let substring_lower = match query {
        Query::Substring(needle) => Some(needle.to_ascii_lowercase()),
        _ => None,
    };
    for (index, record) in delta.live_added() {
        let matched = match query {
            Query::Prefix(prefix) => {
                ascii::starts_with_ci(record.name.as_bytes(), prefix.as_bytes())
            }
            Query::Substring(_) => ascii::contains_ci(
                record.name.as_bytes(),
                substring_lower.as_ref().unwrap().as_bytes(),
            ),
            Query::Regex(_) => regex
                .as_ref()
                .is_some_and(|compiled| compiled.is_match(record.name.as_bytes())),
            Query::PathTerms(_) | Query::FilteredPathTerms { .. } => unreachable!(),
        };
        if matched {
            let combined = arena.len() as u32 + index;
            rank::retain_hit(
                &mut heap,
                sort_key(
                    order,
                    arena,
                    delta,
                    combined,
                    match_quality(query, record.name.as_bytes()),
                    record.name.len() as u32,
                ),
                limit,
            );
        }
    }

    let hits = drain_heap(arena, delta, &mut heap);
    if let (Some(spans), Some(started)) = (spans, finalize_started) {
        spans.finalize_ns = spans
            .finalize_ns
            .saturating_add(started.elapsed().as_nanos() as u64);
    }
    hits
}

#[inline]
fn record_is_dir(arena: &crate::ArchivedArena, delta: &Delta, record: u32) -> bool {
    match record.checked_sub(arena.len() as u32) {
        None => arena.is_dir(record),
        Some(index) => delta.added[index as usize].is_dir,
    }
}

pub(super) fn record_matches_filter(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    record: u32,
    name: &[u8],
    filter: &QueryFilter,
) -> bool {
    let is_dir = record_is_dir(arena, delta, record);
    if matches!(filter.kind, Some(EntryKind::File)) && is_dir
        || matches!(filter.kind, Some(EntryKind::Directory)) && !is_dir
    {
        return false;
    }
    if !filter.extensions.is_empty() {
        if is_dir {
            return false;
        }
        let extension = name.rsplit(|&byte| byte == b'.').next().unwrap_or_default();
        if extension.len() == name.len()
            || !filter
                .extensions
                .iter()
                .any(|wanted| ascii::cmp_ci(extension, wanted.as_bytes()).is_eq())
        {
            return false;
        }
    }
    if filter.min_size.is_some() || filter.max_size.is_some() {
        if !record_size_exact(arena, delta, record) {
            return false;
        }
        let size = record_size_kib(arena, delta, record) as u64 * 1024;
        if filter.min_size.is_some_and(|minimum| size < minimum)
            || filter.max_size.is_some_and(|maximum| size > maximum)
        {
            return false;
        }
    }
    let mtime = record_mtime(arena, delta, record);
    filter.min_mtime.is_none_or(|minimum| mtime >= minimum)
        && filter.max_mtime.is_none_or(|maximum| mtime <= maximum)
}
