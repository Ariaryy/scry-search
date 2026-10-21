//! Cross-volume query selection and bounded refinement caching.

use scry_core::metrics::QuerySpans;
use scry_core::protocol::{Order, QueryKind, ResultEntry};
use scry_core::view::{Hit, SearchOptions};
use scry_core::{IndexView, Query};

use crate::VolumeIndexes;

/// Above this many matches, the daemon stops trusting a query's candidate
/// list as complete: the bounded top-k search inside `IndexView` only keeps
/// every match when the true count is under its limit, so a result this long
/// may have silently dropped matches that a *narrower* follow-up query would
/// need. Caching it would risk a refined query missing a file that exists —
/// the one failure mode this feature must never produce — so that volume's
/// cache entry is left empty instead, and the next keystroke rescans it.
pub(super) const REFINEMENT_CACHE_CAP: usize = 20_000;

/// Worker count for `scry_core::view::search_base_parallel`'s bucket-sharded
/// scan, computed once and reused for the daemon's lifetime rather than
/// re-queried per keystroke. Capped at 8: beyond that, per-shard bucket
/// ranges get small enough that thread coordination overhead starts eating
/// into the win, and it keeps one query from claiming every core on a
/// larger machine while a reindex is also running.
pub(super) fn query_thread_count() -> usize {
    static COUNT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *COUNT.get_or_init(|| {
        // Measurement-only override: lets the daemon be launched at a fixed
        // worker count (e.g. 1, for an apples-to-apples single-threaded
        // comparison) without a separate build.
        if let Some(threads) = std::env::var("SCRY_QUERY_THREADS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
        {
            return threads.max(1);
        }
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8)
    })
}

/// Measurement-only override: `SCRY_NO_REFINEMENT_CACHE=1` makes every query
/// rescan from scratch, for an as-you-type latency comparison against the
/// cached path without a separate build.
pub(super) fn refinement_cache_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("SCRY_NO_REFINEMENT_CACHE").is_none())
}

/// The result of the last refinable query on this connection, per volume, so
/// the next keystroke can filter instead of rescan when it's provably a
/// narrower version of the same query. Lives for the lifetime of one
/// connection: an as-you-type session is exactly the case this optimizes,
/// and nothing about it is meaningful to persist across connections.
#[derive(Default)]
pub(super) struct RefinementCache {
    pub(super) kind: Option<QueryKind>,
    /// The ordering the cached candidates were collected under. A cached set
    /// is only a superset of a refined query's matches for the *same*
    /// ordering: the scan keeps the best `REFINEMENT_CACHE_CAP` by that
    /// ordering, and a different one would have kept different records.
    pub(super) order: Order,
    pub(super) terms: Vec<String>,
    pub(super) per_volume: Vec<Option<VolumeCandidates>>,
    pub(super) last_reused: usize,
    pub(super) last_scanned: usize,
    pub(super) last_bypassed: bool,
    pub(super) last_cancelled: bool,
}

pub(super) struct VolumeCandidates {
    pub(super) generation: u64,
    pub(super) hits: Vec<Hit>,
}

/// The term list a query would need to have matched to be filterable from a
/// cached result later — `None` for kinds that are never cacheable
/// (`Regex`/`Wildcard`: a longer pattern can *grow* the match set, so no
/// subset relationship can be assumed) or `ShareIndex` (not a search at all).
pub(super) fn refinable_terms(kind: QueryKind, query: &Query) -> Option<Vec<String>> {
    match (kind, query) {
        (QueryKind::Prefix, Query::Prefix(pattern))
        | (QueryKind::Substring, Query::Substring(pattern)) => Some(vec![pattern.clone()]),
        (QueryKind::PathTerms, Query::PathTerms(terms)) => Some(terms.clone()),
        _ => None,
    }
}

/// True when `new_terms` can only match a subset of what `old_terms` matched:
/// every old term, in order, is a case-insensitive prefix of the
/// correspondingly-positioned new term, and no old term is missing from the
/// new list. Extra new terms beyond `old_terms.len()` are fine — an
/// additional AND-ed term only shrinks the match set further.
pub(super) fn is_refinement(old_terms: &[String], new_terms: &[String]) -> bool {
    !old_terms.is_empty()
        && new_terms.len() >= old_terms.len()
        && old_terms.iter().zip(new_terms.iter()).all(|(old, new)| {
            new.len() >= old.len()
                && new.as_bytes()[..old.len()].eq_ignore_ascii_case(old.as_bytes())
        })
}

/// Re-checks a cached hit against a refined query without touching the
/// index, mirroring the matching rules `search_base`/`search_path_terms` use
/// so a filtered cache and a full rescan never disagree.
pub(super) fn matches_refined(
    view: &IndexView,
    hit: Hit,
    kind: QueryKind,
    terms_lower: &[Vec<u8>],
    name: &mut Vec<u8>,
) -> bool {
    match kind {
        QueryKind::Prefix => {
            view.name_into(hit.record, name);
            name.len() >= terms_lower[0].len()
                && name[..terms_lower[0].len()].eq_ignore_ascii_case(&terms_lower[0])
        }
        QueryKind::Substring => {
            view.name_into(hit.record, name);
            scry_core::ascii::contains_ci(name, &terms_lower[0])
        }
        QueryKind::PathTerms => view.matches_path_terms_lower(hit.record, terms_lower, name),
        QueryKind::Wildcard | QueryKind::ShareIndex | QueryKind::QueryStats => false,
    }
}

/// As `search_indexes_cancellable`, but for a refinable query kind, checks
/// whether this connection's cache holds the previous, broader query's
/// complete match set per volume and filters it in memory instead of
/// rescanning that volume. A volume whose index generation moved, or whose
/// cached set hit `REFINEMENT_CACHE_CAP` last time, is rescanned on its own —
/// one volume reindexing must not force a rescan of the others.
#[cfg(test)]
pub(super) fn search_indexes_with_cache(
    indexes: &VolumeIndexes,
    kind: QueryKind,
    query: &Query,
    options: SearchOptions,
    cancel: scry_core::Cancellation,
    cache: &mut RefinementCache,
) -> Vec<ResultEntry> {
    search_indexes_with_cache_with_spans(
        indexes,
        kind,
        query,
        options,
        cancel,
        cache,
        &mut QuerySpans::default(),
    )
}

pub(super) fn search_indexes_with_cache_with_spans(
    indexes: &VolumeIndexes,
    kind: QueryKind,
    query: &Query,
    options: SearchOptions,
    cancel: scry_core::Cancellation,
    cache: &mut RefinementCache,
    spans: &mut QuerySpans,
) -> Vec<ResultEntry> {
    let SearchOptions { limit, order } = options;
    cache.last_reused = 0;
    cache.last_scanned = 0;
    cache.last_bypassed = false;
    cache.last_cancelled = false;
    if limit == 0 || cancel.is_cancelled() {
        return Vec::new();
    }
    let Some(new_terms) = refinable_terms(kind, query).filter(|_| refinement_cache_enabled())
    else {
        cache.last_bypassed = true;
        cache.last_scanned = indexes.len();
        return search_indexes_cancellable_with_spans(indexes, query, options, cancel, spans);
    };
    let refine =
        cache.kind == Some(kind) && cache.order == order && is_refinement(&cache.terms, &new_terms);
    let terms_lower: Vec<Vec<u8>> = new_terms
        .iter()
        .map(|term| term.as_bytes().to_ascii_lowercase())
        .collect();
    if !refine || cache.per_volume.len() != indexes.len() {
        cache.per_volume.clear();
        cache.per_volume.resize_with(indexes.len(), || None);
    }

    // Overscanning to `REFINEMENT_CACHE_CAP` only pays for itself if a later
    // keystroke actually filters the result, and every hit it collects beyond
    // `limit` costs a `full_path` reconstruction (~3 µs) plus a `String`. A
    // one-shot `scry <query>` opens a connection, asks once and exits, so it
    // would pay that for nothing. Widen only once this connection has already
    // served a refinable query — that is what an as-you-type session looks
    // like, and it is the only case where the cache can be read back.
    let seen_refinable_query = cache.kind.is_some();
    let scan_limit = if seen_refinable_query {
        REFINEMENT_CACHE_CAP
    } else {
        limit
    };

    let mut views = Vec::with_capacity(indexes.len());
    let mut merged = Vec::new();
    let mut next_per_volume = Vec::with_capacity(indexes.len());
    for (i, index) in indexes.iter().enumerate() {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let view = index.store.load_full();
        let reused = refine
            .then(|| cache.per_volume[i].as_ref())
            .flatten()
            .filter(|cached| cached.generation == view.generation);
        let mut name = Vec::new();
        let hits = if let Some(cached) = reused {
            cache.last_reused += 1;
            cached
                .hits
                .iter()
                .copied()
                .filter_map(|mut hit| {
                    if !matches_refined(&view, hit, kind, &terms_lower, &mut name) {
                        return None;
                    }
                    if order == Order::Relevance {
                        // Prefix/substring refinement leaves the leaf name
                        // in `name`; path matching walks ancestors, so load
                        // the leaf once here before rebuilding its rank.
                        if kind == QueryKind::PathTerms {
                            view.name_into(hit.record, &mut name);
                        }
                        let key = scry_core::rank::relevance_key(
                            hit_quality(query, &terms_lower, &name),
                            name.len() as u32,
                            hit.record,
                        );
                        hit.rank_bits = scry_core::rank::key_rank_bits(key);
                    }
                    Some(hit)
                })
                .collect::<Vec<_>>()
        } else {
            cache.last_scanned += 1;
            view.search_hits_cancellable_with_spans(
                query,
                SearchOptions::ordered(scan_limit, order),
                Some(cancel),
                query_thread_count(),
                Some(&mut *spans),
            )
        };
        if cancel.is_cancelled() {
            return Vec::new();
        }
        merged.extend(hits.iter().copied().map(|hit| (i, hit)));
        // Only a set scanned at the full cap is known to be a superset of what
        // a refined query could match. A set truncated at `limit` is not, and
        // caching it would let a later keystroke miss real hits.
        next_per_volume.push(
            (scan_limit == REFINEMENT_CACHE_CAP && hits.len() < REFINEMENT_CACHE_CAP).then(|| {
                VolumeCandidates {
                    generation: view.generation,
                    hits,
                }
            }),
        );
        views.push(view);
    }
    cache.kind = Some(kind);
    cache.order = order;
    cache.terms = new_terms;
    cache.per_volume = next_per_volume;

    let merge_started = std::time::Instant::now();
    rank_sort_truncate_hits(&mut merged, limit);
    spans.merge_ns = spans
        .merge_ns
        .saturating_add(merge_started.elapsed().as_nanos() as u64);
    let started = std::time::Instant::now();
    let entries: Vec<_> = merged
        .into_iter()
        .map(|(volume, hit)| views[volume].materialize_one(&hit))
        .collect();
    spans.materialize_ns = spans
        .materialize_ns
        .saturating_add(started.elapsed().as_nanos() as u64);
    spans.emitted = spans.emitted.saturating_add(entries.len() as u64);
    entries
}

/// Fans a query out across every volume's index and merges by rank, in one
/// bounded top-k pass per volume. Abandons the fan-out (returning an empty
/// result) once `cancel` reports this request was superseded.
#[cfg(test)]
pub(super) fn search_indexes_cancellable(
    indexes: &VolumeIndexes,
    query: &Query,
    options: SearchOptions,
    cancel: scry_core::Cancellation,
) -> Vec<ResultEntry> {
    search_indexes_cancellable_with_spans(
        indexes,
        query,
        options,
        cancel,
        &mut QuerySpans::default(),
    )
}

pub(super) fn search_indexes_cancellable_with_spans(
    indexes: &VolumeIndexes,
    query: &Query,
    options: SearchOptions,
    cancel: scry_core::Cancellation,
    spans: &mut QuerySpans,
) -> Vec<ResultEntry> {
    if options.limit == 0 || cancel.is_cancelled() {
        return Vec::new();
    }
    let mut views = Vec::with_capacity(indexes.len());
    let mut hits_by_volume = Vec::new();
    for (volume, index) in indexes.iter().enumerate() {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let view = index.store.load_full();
        let hits = view.search_hits_cancellable_with_spans(
            query,
            options,
            Some(cancel),
            query_thread_count(),
            Some(&mut *spans),
        );
        hits_by_volume.extend(hits.into_iter().map(|hit| (volume, hit)));
        views.push(view);
    }
    if cancel.is_cancelled() {
        return Vec::new();
    }
    let merge_started = std::time::Instant::now();
    rank_sort_truncate_hits(&mut hits_by_volume, options.limit);
    spans.merge_ns = spans
        .merge_ns
        .saturating_add(merge_started.elapsed().as_nanos() as u64);
    let started = std::time::Instant::now();
    let entries: Vec<_> = hits_by_volume
        .into_iter()
        .map(|(volume, hit)| views[volume].materialize_one(&hit))
        .collect();
    spans.materialize_ns = spans
        .materialize_ns
        .saturating_add(started.elapsed().as_nanos() as u64);
    spans.emitted = spans.emitted.saturating_add(entries.len() as u64);
    entries
}

/// Rank cross-volume hits before paths are reconstructed. `rank_bits` carries
/// the comparable half of the key computed during the per-volume scan; the
/// stable volume slot and then its local record finish otherwise-equal keys.
pub(super) fn rank_sort_truncate_hits(hits: &mut Vec<(usize, Hit)>, limit: usize) {
    if hits.len() <= 1 {
        hits.truncate(limit);
        return;
    }
    hits.sort_unstable_by_key(|(volume, hit)| (hit.rank_bits, *volume, hit.record));
    hits.truncate(limit);
}

/// `terms_lower` is the request's once-normalized term list: typed terms keep
/// their case, and `contains_ci` folds only the haystack.
pub(super) fn hit_quality(query: &Query, terms_lower: &[Vec<u8>], name: &[u8]) -> u8 {
    match query {
        Query::Prefix(pattern) | Query::Substring(pattern) => {
            if name.eq_ignore_ascii_case(pattern.as_bytes()) {
                0
            } else if name.len() >= pattern.len()
                && name[..pattern.len()].eq_ignore_ascii_case(pattern.as_bytes())
            {
                1
            } else {
                2
            }
        }
        Query::PathTerms(_) | Query::FilteredPathTerms { .. } => terms_lower
            .iter()
            .filter(|term| !scry_core::ascii::contains_ci(name, term))
            .count() as u8,
        Query::Regex(_) => 2,
    }
}
