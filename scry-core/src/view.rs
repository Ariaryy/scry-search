//! Coherent immutable index views and public search/materialization APIs.

use std::sync::Arc;

use crate::cancel::Cancellation;
use crate::delta::{Delta, ParentRef};
use crate::metrics::QuerySpans;
use crate::protocol::ResultEntry;
use crate::query::Query;
use crate::rank::Order;
use crate::store::ArenaStore;
use crate::PARENT_NONE;

mod compaction;
mod path_search;
mod ranked;

pub use path_search::search_path_terms;
use path_search::{search_path_terms_cancellable, search_path_terms_filtered_cancellable};
use ranked::{search_ranked, search_ranked_cancellable_with_spans};

#[cfg(test)]
mod tests;

/// Ceiling on any caller-supplied result limit. The bounded top-k heap is the
/// only thing standing between a one-character query and an allocation
/// proportional to the whole index, so the limit is clamped at the API
/// boundary rather than trusted.
pub const MAX_LIMIT: usize = 100_000;

/// What to return and in what order.
#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub limit: usize,
    pub order: Order,
}

impl SearchOptions {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.min(MAX_LIMIT),
            order: Order::default(),
        }
    }

    pub fn ordered(limit: usize, order: Order) -> Self {
        Self {
            limit: limit.min(MAX_LIMIT),
            order,
        }
    }
}

/// A match, before its path has been reconstructed.
///
/// Every field here is either already in the key or a single indexed read from
/// a column; the path is not — it costs a parent-chain walk and a `String` per
/// result, which measured at ~3.5 µs each. A caller that counts matches,
/// aggregates sizes, or renders paths lazily as the user scrolls should take
/// `Hit`s and call [`IndexView::path_of`] only for what it displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// Index into the combined base-then-delta record space.
    pub record: u32,
    /// Ordering-significant high half of the key used to retain this hit.
    /// This is comparable across volumes; `record` is not.
    pub rank_bits: u32,
    pub size: u64,
    pub mtime: u32,
    pub is_dir: bool,
    pub size_exact: bool,
}

/// Immutable base-and-overlay pair published through one atomic pointer.
pub struct IndexView {
    pub base: Arc<ArenaStore>,
    pub delta: Arc<Delta>,
    pub generation: u64,
    pub journal_id: u64,
    pub next_usn: i64,
    pub volume_serial: u64,
}

impl IndexView {
    pub fn new(base: Arc<ArenaStore>) -> Self {
        let records = base.archived().len();
        let journal_id = base.archived().journal_id;
        let next_usn = base.archived().next_usn;
        let volume_serial = base.archived().volume_serial;
        let delta = Arc::new(Delta::new(records));
        Self {
            base,
            delta,
            generation: fresh_generation(),
            journal_id,
            next_usn,
            volume_serial,
        }
    }

    pub fn len(&self) -> usize {
        self.base.archived().len() - self.delta.tombstones.count_ones() as usize
            + self.delta.live_added().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Search the sorted base first, followed by live overlay records in
    /// creation order. The combined result is intentionally not globally
    /// sorted while the overlay remains uncompacted.
    pub fn search(&self, query: &Query, limit: usize) -> Vec<ResultEntry> {
        self.search_cancellable(query, limit, None)
    }

    /// Match and rank without reconstructing any paths.
    ///
    /// This is the primitive the other `search*` methods are built on. A caller
    /// that doesn't need every path — a counter, a size aggregator, a list that
    /// renders lazily — should use this and call [`Self::path_of`] for the rows
    /// it actually shows.
    pub fn search_hits(&self, query: &Query, options: SearchOptions) -> Vec<Hit> {
        self.search_hits_cancellable(query, options, None, 1)
    }

    /// As [`Self::search_hits`], with cancellation and an optional thread pool
    /// for the unfiltered-scan fallback.
    pub fn search_hits_cancellable(
        &self,
        query: &Query,
        options: SearchOptions,
        cancel: Option<Cancellation>,
        threads: usize,
    ) -> Vec<Hit> {
        self.search_hits_cancellable_with_spans(query, options, cancel, threads, None)
    }

    /// As [`Self::search_hits_cancellable`], collecting phase timings only
    /// when `spans` is supplied.
    pub fn search_hits_cancellable_with_spans(
        &self,
        query: &Query,
        options: SearchOptions,
        cancel: Option<Cancellation>,
        threads: usize,
        spans: Option<&mut QuerySpans>,
    ) -> Vec<Hit> {
        if let Query::PathTerms(terms) = query {
            let started = spans.as_ref().map(|_| std::time::Instant::now());
            let hits = search_path_terms_cancellable(
                self.base.archived(),
                &self.delta,
                terms,
                options,
                cancel,
                threads,
            );
            if let (Some(spans), Some(started)) = (spans, started) {
                spans.select_ns = spans
                    .select_ns
                    .saturating_add(started.elapsed().as_nanos() as u64);
                spans.candidates = spans.candidates.saturating_add(hits.len() as u64);
            }
            return hits;
        }
        if let Query::FilteredPathTerms { terms, filter } = query {
            let started = spans.as_ref().map(|_| std::time::Instant::now());
            let hits = search_path_terms_filtered_cancellable(
                self.base.archived(),
                &self.delta,
                terms,
                filter,
                options,
                cancel,
                threads,
            );
            if let (Some(spans), Some(started)) = (spans, started) {
                spans.select_ns = spans
                    .select_ns
                    .saturating_add(started.elapsed().as_nanos() as u64);
                spans.candidates = spans.candidates.saturating_add(hits.len() as u64);
            }
            return hits;
        }
        search_ranked_cancellable_with_spans(
            self.base.archived(),
            &self.delta,
            query,
            options,
            cancel,
            threads,
            spans,
        )
    }

    /// As [`Self::search_hits`], but reconstructs every path.
    pub fn search_with(&self, query: &Query, options: SearchOptions) -> Vec<ResultEntry> {
        self.materialize(&self.search_hits(query, options))
    }

    /// The full path of one hit's record.
    pub fn path_of(&self, record: u32) -> String {
        let mut buf = String::new();
        path_of_into(self.base.archived(), &self.delta, record, &mut buf);
        buf
    }

    /// Decode a record's leaf name into `output` without reconstructing its
    /// parent path.
    pub fn name_into(&self, record: u32, output: &mut Vec<u8>) {
        match record.checked_sub(self.base.archived().len() as u32) {
            None => self.base.archived().name_into(record, output),
            Some(index) => {
                output.clear();
                if let Some(record) = self.delta.added.get(index as usize) {
                    output.extend_from_slice(record.name.as_bytes());
                }
            }
        }
    }

    /// Test terms against a record and its ancestors without allocating a
    /// full path. The hop cap matches [`crate::ArchivedArena::full_path`].
    pub fn matches_path_terms(&self, record: u32, terms: &[String], name: &mut Vec<u8>) -> bool {
        let terms_lower: Vec<Vec<u8>> = terms
            .iter()
            .map(|term| term.as_bytes().to_ascii_lowercase())
            .collect();
        self.matches_path_terms_lower(record, &terms_lower, name)
    }

    /// Allocation-free refinement primitive for callers that normalize the
    /// term list once per request. Every `terms_lower` entry must already be
    /// ASCII-lowercase.
    pub fn matches_path_terms_lower(
        &self,
        record: u32,
        terms_lower: &[Vec<u8>],
        name: &mut Vec<u8>,
    ) -> bool {
        if terms_lower.is_empty() || terms_lower.len() > crate::terms::MAX_TERMS {
            return false;
        }
        let expected = (1u16 << terms_lower.len()) - 1;
        let mut matched = 0u16;
        let mut current = record;
        for _ in 0..512 {
            self.name_into(current, name);
            for (index, term_lower) in terms_lower.iter().enumerate() {
                if crate::ascii::contains_ci(name, term_lower) {
                    matched |= 1 << index;
                }
            }
            if matched == expected {
                return true;
            }
            let base_len = self.base.archived().len() as u32;
            if current < base_len {
                let parent = self.base.archived().tree_parent(current);
                if parent == PARENT_NONE || parent >= base_len {
                    return false;
                }
                current = parent;
            } else {
                let Some(delta) = self.delta.added.get((current - base_len) as usize) else {
                    return false;
                };
                match delta.parent {
                    ParentRef::Base(parent) => current = parent,
                    ParentRef::Delta(parent) => current = base_len + parent,
                    ParentRef::None => return false,
                }
            }
        }
        false
    }

    /// Reconstruct the path of every hit. See [`Hit`] for why this is a
    /// separate step.
    pub fn materialize(&self, hits: &[Hit]) -> Vec<ResultEntry> {
        materialize_hits(self.base.archived(), &self.delta, hits)
    }

    /// Reconstruct one result after selection has already bounded the set.
    pub fn materialize_one(&self, hit: &Hit) -> ResultEntry {
        let mut buf = String::new();
        materialize(self.base.archived(), &self.delta, hit, &mut buf)
    }

    /// As `search`, but abandons the scan (returning an empty result) once
    /// `cancel` reports a newer request superseded this one on the same
    /// connection. `cancel` is `None` for a one-shot caller, which reduces to
    /// plain `search`.
    pub fn search_cancellable(
        &self,
        query: &Query,
        limit: usize,
        cancel: Option<Cancellation>,
    ) -> Vec<ResultEntry> {
        self.search_cancellable_pooled(query, limit, cancel, 1)
    }

    /// As `search_cancellable`, but for a `Substring`/`Regex` query that
    /// would fall back to an unfiltered scan, spreads that scan across up to
    /// `threads` threads (see `query::search_base_parallel`). `threads = 1`
    /// is exactly `search_cancellable`'s behavior. Used only by the daemon,
    /// which owns sizing the thread count to the machine.
    pub fn search_cancellable_pooled(
        &self,
        query: &Query,
        limit: usize,
        cancel: Option<Cancellation>,
        threads: usize,
    ) -> Vec<ResultEntry> {
        let options = SearchOptions::new(limit);
        self.materialize(&self.search_hits_cancellable(query, options, cancel, threads))
    }

    pub fn delta_path(&self, index: u32) -> String {
        delta_path(self.base.archived(), &self.delta, index)
    }
}

pub fn fresh_generation() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Search a validated archived base together with its coherent delta overlay.
pub fn search_archived_with_delta(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    query: &Query,
    options: SearchOptions,
) -> Vec<ResultEntry> {
    let hits = match query {
        Query::PathTerms(terms) => search_path_terms(arena, delta, terms, options),
        Query::FilteredPathTerms { terms, filter } => {
            search_path_terms_filtered_cancellable(arena, delta, terms, filter, options, None, 1)
        }
        _ => search_ranked(arena, delta, query, options),
    };
    materialize_hits(arena, delta, &hits)
}

/// Reconstruct the path of every hit. Split out of the search functions so a
/// caller can rank first and pay for paths only where it needs them — see
/// [`Hit`].
pub fn materialize_hits(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    hits: &[Hit],
) -> Vec<ResultEntry> {
    // Reused across the whole batch: `path_of_into` clears and refills it
    // per hit, so its backing capacity only has to grow once instead of once
    // per hit.
    let mut buf = String::new();
    hits.iter()
        .map(|hit| materialize(arena, delta, hit, &mut buf))
        .collect()
}

/// Reconstruct the path of one hit. Separated from matching and ranking
/// because it is the expensive part — see [`Hit`]. `buf` is scratch space
/// the caller may reuse across several hits; its contents on entry do not
/// matter.
fn materialize(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    hit: &Hit,
    buf: &mut String,
) -> ResultEntry {
    path_of_into(arena, delta, hit.record, buf);
    ResultEntry {
        path: buf.clone(),
        size: hit.size,
        mtime: hit.mtime,
        is_dir: hit.is_dir,
        size_exact: hit.size_exact,
    }
}

fn path_of_into(arena: &crate::ArchivedArena, delta: &Delta, record: u32, buf: &mut String) {
    match record.checked_sub(arena.len() as u32) {
        None => arena.full_path_into(record, '\\', buf),
        Some(index) => {
            buf.clear();
            buf.push_str(&delta_path(arena, delta, index));
        }
    }
}

fn delta_path(arena: &crate::ArchivedArena, delta: &Delta, mut index: u32) -> String {
    let mut parts = Vec::new();
    let mut base_parent = None;
    for _ in 0..512 {
        let record = &delta.added[index as usize];
        parts.push(record.name.clone());
        match record.parent {
            ParentRef::Base(parent) => {
                base_parent = Some(parent);
                break;
            }
            ParentRef::Delta(parent) => index = parent,
            ParentRef::None => break,
        }
    }
    parts.reverse();
    let suffix = parts.join("\\");
    match base_parent {
        Some(parent) => format!("{}\\{suffix}", arena.full_path(parent, '\\')),
        None => suffix,
    }
}
