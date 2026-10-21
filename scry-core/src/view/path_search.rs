//! DFS interval matching and bounded path-query scratch.

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::{cell::RefCell, collections::BinaryHeap};

use super::ranked::{drain_heap, record_matches_filter, sort_key};
use super::{Hit, SearchOptions};
use crate::ascii;
use crate::cancel::Cancellation;
use crate::delta::{Delta, ParentRef};
use crate::query::{is_cancelled_periodically, QueryFilter};
use crate::rank::{self, Order};
use crate::PARENT_NONE;

/// Tiny final DFS sets use direct name decoding without candidate staging.
pub(super) const SPARSE_PATH_HITS: u64 = 1024;

/// Retained per-thread query scratch, excluding caller-owned output and live
/// in-flight work. Large queries release their buffers before returning.
pub(super) const PATH_SCRATCH_RETAINED_BYTES: usize = 1024 * 1024;

/// Sparse enumeration stages only record IDs, in bounded batches. No paths,
/// names, or per-match objects survive a bucket visit.
pub(super) const PATH_RECORD_BATCH: usize = 4096;

/// Prefer bounded batches when staging all IDs would use less space than a
/// whole-index bit per record. Batching caps the actual staging at 16 KiB even
/// when a selective query has more survivors than fit in one batch.
pub(super) fn use_path_record_batches(records: usize, positions: u64) -> bool {
    positions <= (records / (8 * std::mem::size_of::<u32>())) as u64
}

fn visit_path_record_batch(
    arena: &crate::ArchivedArena,
    records: &mut [u32],
    name: &mut Vec<u8>,
    cancel: Option<Cancellation>,
    visit: &mut impl FnMut(u32, &[u8], bool),
) -> bool {
    records.sort_unstable();
    let mut remaining = &*records;
    while let Some(&first) = remaining.first() {
        if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
            return false;
        }
        let start = first / crate::record::BUCKET_SIZE as u32 * crate::record::BUCKET_SIZE as u32;
        let count =
            remaining.partition_point(|&record| record - start < crate::record::BUCKET_SIZE as u32);
        let (bucket, rest) = remaining.split_at(count);
        let last = *bucket.last().unwrap();
        let mask = bucket
            .iter()
            .fold(0u32, |mask, &record| mask | (1 << (record - start)));
        let mut can_reuse = false;
        arena.for_each_name_in_buf_with_repeats(
            first..last + 1,
            name,
            |record, record_name, repeated| {
                if !repeated {
                    can_reuse = false;
                }
                if mask & (1 << (record - start)) != 0 {
                    visit(record, record_name, can_reuse);
                    can_reuse = true;
                }
                if record == last {
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            },
        );
        remaining = rest;
    }
    true
}

/// Visit exactly the final DFS set in bucket order (within a bounded batch for
/// sparse results). Candidate staging is separate from bounded top-k retention.
pub(super) fn visit_path_candidate_names(
    arena: &crate::ArchivedArena,
    runs: &crate::intervals::IntervalSet,
    name: &mut Vec<u8>,
    cancel: Option<Cancellation>,
    mut visit: impl FnMut(u32, &[u8], bool),
) -> bool {
    let mut checked = 0;
    if use_path_record_batches(arena.len(), runs.len_positions()) {
        let mut records =
            Vec::with_capacity((runs.len_positions() as usize).min(PATH_RECORD_BATCH));
        for &(start, end) in runs.runs() {
            for position in start..end {
                if is_cancelled_periodically(cancel, &mut checked) {
                    return false;
                }
                records.push(arena.dfs_record(position));
                if records.len() == PATH_RECORD_BATCH {
                    if !visit_path_record_batch(arena, &mut records, name, cancel, &mut visit) {
                        return false;
                    }
                    records.clear();
                }
            }
        }
        return visit_path_record_batch(arena, &mut records, name, cancel, &mut visit);
    }

    // Dense results retain the linear bitmap walk. Keep this local so index-
    // sized storage never remains in a long-lived connection's scratch.
    let Some(matches) = path_record_bitmap(arena, runs, cancel) else {
        return false;
    };
    visit_path_bitmap_names(
        arena,
        &matches,
        name,
        || cancel.is_some_and(|cancel| cancel.is_cancelled()),
        visit,
    )
}

/// Poll before every bitmap bucket, including empty ones. Each decode covers
/// at most BUCKET_SIZE records, so cancellation cannot skip a long empty run
/// or accumulate a record counter in bucket units. The final poll also covers
/// cancellation during the last (possibly partial) bucket.
pub(super) fn visit_path_bitmap_names(
    arena: &crate::ArchivedArena,
    matches: &[u8],
    name: &mut Vec<u8>,
    mut is_cancelled: impl FnMut() -> bool,
    mut visit: impl FnMut(u32, &[u8], bool),
) -> bool {
    const BUCKET_BYTES: usize = crate::record::BUCKET_SIZE / 8;
    for (bucket, bits) in matches.chunks(BUCKET_BYTES).enumerate() {
        if is_cancelled() {
            return false;
        }
        if bits.iter().all(|&byte| byte == 0) {
            continue;
        }
        let start = (bucket * crate::record::BUCKET_SIZE) as u32;
        let end = start + crate::record::BUCKET_SIZE as u32;
        let mut can_reuse = false;
        arena.for_each_name_in_buf_with_repeats(
            start..end,
            name,
            |record, record_name, repeated| {
                if !repeated {
                    can_reuse = false;
                }
                let offset = (record - start) as usize;
                if bits[offset / 8] & (1 << (offset % 8)) != 0 {
                    visit(record, record_name, can_reuse);
                    can_reuse = true;
                }
                std::ops::ControlFlow::Continue(())
            },
        );
    }
    !is_cancelled()
}

/// Convert DFS intervals to record membership once. Dense workers borrow the
/// same bitmap; it is released with the query rather than retained in scratch.
pub(super) fn path_record_bitmap(
    arena: &crate::ArchivedArena,
    runs: &crate::intervals::IntervalSet,
    cancel: Option<Cancellation>,
) -> Option<Vec<u8>> {
    if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
        return None;
    }
    let mut matches = vec![0u8; arena.len().div_ceil(8)];
    let mut checked = 0;
    for &(start, end) in runs.runs() {
        for position in start..end {
            if is_cancelled_periodically(cancel, &mut checked) {
                return None;
            }
            let record = arena.dfs_record(position);
            matches[record as usize / 8] |= 1 << (record % 8);
        }
    }
    if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
        None
    } else {
        Some(matches)
    }
}

#[derive(Default)]
pub(super) struct PathSearchScratch {
    /// One coalesced [`crate::intervals::IntervalSet`] of DFS positions per
    /// term, built from independent per-term scans — never merged at the
    /// trigram-block level, since different terms may be satisfied by
    /// different ancestor records.
    pub(super) term_sets: Vec<crate::intervals::IntervalSet>,
    pub(super) fold_a: crate::intervals::IntervalSet,
    pub(super) fold_b: crate::intervals::IntervalSet,
    pub(super) candidate_blocks: Vec<u32>,
    pub(super) candidate_bitmap: Vec<u8>,
    pub(super) trigram_hashes: Vec<usize>,
    pub(super) own_matches: Vec<u8>,
    pub(super) heap: BinaryHeap<u64>,
    pub(super) name: Vec<u8>,
}

impl PathSearchScratch {
    pub(super) fn allocated_bytes(&self) -> usize {
        self.term_sets.capacity() * std::mem::size_of::<crate::intervals::IntervalSet>()
            + self
                .term_sets
                .iter()
                .map(crate::intervals::IntervalSet::allocated_bytes)
                .sum::<usize>()
            + self.fold_a.allocated_bytes()
            + self.fold_b.allocated_bytes()
            + self.candidate_blocks.capacity() * std::mem::size_of::<u32>()
            + self.candidate_bitmap.capacity()
            + self.trigram_hashes.capacity() * std::mem::size_of::<usize>()
            + self.own_matches.capacity()
            + self.heap.capacity() * std::mem::size_of::<u64>()
            + self.name.capacity()
    }

    fn release_excess(&mut self) {
        if self.allocated_bytes() > PATH_SCRATCH_RETAINED_BYTES {
            *self = Self::default();
        }
    }

    fn reset(&mut self, term_count: usize, limit: usize) {
        self.term_sets.resize_with(term_count, Default::default);
        for set in self.term_sets.iter_mut().take(term_count) {
            set.clear();
        }
        self.fold_a.clear();
        self.fold_b.clear();
        self.candidate_blocks.clear();
        self.candidate_bitmap.clear();
        self.trigram_hashes.clear();
        self.own_matches.clear();
        self.heap.clear();
        self.name.clear();
        let target = limit.min(4096);
        if self.heap.capacity() < target {
            self.heap.reserve(target);
        }
    }
}

thread_local! {
    pub(super) static PATH_SEARCH_SCRATCH: RefCell<PathSearchScratch> =
        RefCell::new(PathSearchScratch::default());
    #[cfg(test)]
    pub(super) static TERM_INTERVAL_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) static PATH_TERM_SCAN_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
pub(super) static PATH_TERM_COALESCE_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
pub(super) static PATH_TERM_ENUMERATE_NS: AtomicU64 = AtomicU64::new(0);

pub fn search_path_terms(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    terms: &[String],
    options: SearchOptions,
) -> Vec<Hit> {
    search_path_terms_cancellable(arena, delta, terms, options, None, 1)
}

pub(super) fn search_path_terms_cancellable(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    terms: &[String],
    options: SearchOptions,
    cancel: Option<Cancellation>,
    threads: usize,
) -> Vec<Hit> {
    search_path_terms_filtered_cancellable(
        arena,
        delta,
        terms,
        &QueryFilter::default(),
        options,
        cancel,
        threads,
    )
}

pub(super) fn search_path_terms_filtered_cancellable(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    terms: &[String],
    filter: &QueryFilter,
    options: SearchOptions,
    cancel: Option<Cancellation>,
    threads: usize,
) -> Vec<Hit> {
    PATH_SEARCH_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        let hits = search_path_terms_with_scratch(
            arena,
            delta,
            terms,
            filter,
            options,
            &mut scratch,
            true,
            cancel,
            threads,
        );
        scratch.release_excess();
        hits
    })
}

/// Scan the base arena for every live record whose own name contains
/// `term_lower`, folding its DFS interval into `set`: the whole subtree span
/// for a directory match (so descendants inherit the term through plain
/// interval containment), a single point for a file match. Returns `false`
/// if the scan was cancelled partway through, in which case `set` must be
/// discarded by the caller.
fn build_short_single_term_set(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    term_lower: &[u8],
    set: &mut crate::intervals::IntervalSet,
    own_matches: &mut Vec<u8>,
    cancel: Option<Cancellation>,
    threads: usize,
) -> bool {
    own_matches.resize(arena.len().div_ceil(8), 0);
    own_matches.fill(0);
    #[cfg(test)]
    let scan_started = std::time::Instant::now();
    if threads <= 1 || arena.len() < crate::record::BUCKET_SIZE * 2 {
        let mut checked = 0u32;
        let mut cancelled = false;
        let mut matched = false;
        arena.for_each_name_with_repeats(|record, name, repeated| {
            if is_cancelled_periodically(cancel, &mut checked) {
                cancelled = true;
                return std::ops::ControlFlow::Break(());
            }
            if !repeated {
                matched = ascii::contains_ci(name, term_lower);
            }
            if !delta.tombstones.get(record) && matched {
                own_matches[record as usize / 8] |= 1 << (record % 8);
            }
            std::ops::ControlFlow::Continue(())
        });
        if cancelled {
            return false;
        }
    } else {
        let ranges: Vec<_> = crate::query::bucket_shards(arena.len(), threads).collect();
        let locals: Vec<Vec<u8>> = ranges
            .iter()
            .map(|&(start, end)| vec![0; (end - start).div_ceil(8) as usize])
            .collect();
        let shards: Vec<(u32, Vec<u8>, bool)> = std::thread::scope(|scope| {
            ranges
                .into_iter()
                .zip(locals)
                .map(|((start, end), mut local)| {
                    std::thread::Builder::new()
                        .stack_size(64 * 1024)
                        .spawn_scoped(scope, move || {
                            let mut checked = 0u32;
                            let mut cancelled = false;
                            let mut matched = false;
                            arena.for_each_name_in_with_repeats(
                                start..end,
                                |record, name, repeated| {
                                    if is_cancelled_periodically(cancel, &mut checked) {
                                        cancelled = true;
                                        return std::ops::ControlFlow::Break(());
                                    }
                                    if !repeated {
                                        matched = ascii::contains_ci(name, term_lower);
                                    }
                                    if !delta.tombstones.get(record) && matched {
                                        let local_record = record - start;
                                        local[local_record as usize / 8] |= 1 << (local_record % 8);
                                    }
                                    std::ops::ControlFlow::Continue(())
                                },
                            );
                            (start, local, cancelled)
                        })
                        .expect("short path worker spawn failed")
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        if shards.iter().any(|(_, _, cancelled)| *cancelled) {
            return false;
        }
        for (start, local, _) in shards {
            let start_byte = start as usize / 8;
            own_matches[start_byte..start_byte + local.len()].copy_from_slice(&local);
        }
    }
    if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
        return false;
    }
    #[cfg(test)]
    PATH_TERM_SCAN_NS.fetch_add(
        scan_started.elapsed().as_nanos() as u64,
        AtomicOrdering::Relaxed,
    );
    #[cfg(test)]
    let coalesce_started = std::time::Instant::now();
    let mut inherited_end = 0u32;
    let mut checked = 0u32;
    for position in 0..arena.len() as u32 {
        if is_cancelled_periodically(cancel, &mut checked) {
            return false;
        }
        let record = arena.dfs_record(position);
        let own = own_matches[record as usize / 8] & (1 << (record % 8)) != 0;
        if own && arena.is_dir(record) {
            inherited_end = inherited_end.max(arena.subtree(record).end);
        }
        if own || position < inherited_end {
            set.push_ordered_point(position);
        }
    }
    #[cfg(test)]
    PATH_TERM_COALESCE_NS.fetch_add(
        coalesce_started.elapsed().as_nanos() as u64,
        AtomicOrdering::Relaxed,
    );
    true
}

#[allow(clippy::too_many_arguments)]
fn build_term_interval_set(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    term_lower: &[u8],
    use_filter: bool,
    set: &mut crate::intervals::IntervalSet,
    candidate_blocks: &mut Vec<u32>,
    candidate_bitmap: &mut Vec<u8>,
    trigram_hashes: &mut Vec<usize>,
    cancel: Option<Cancellation>,
    checked: &mut u32,
) -> bool {
    #[cfg(test)]
    TERM_INTERVAL_BUILDS.with(|count| count.set(count.get() + 1));
    #[cfg(test)]
    let scan_started = std::time::Instant::now();
    let filtered = use_filter
        && arena.candidate_blocks_into(
            term_lower,
            candidate_blocks,
            candidate_bitmap,
            trigram_hashes,
        );
    let mut matched = false;
    let mut cancelled = false;
    let mut visit = |record, name: &[u8], repeated: bool| {
        if is_cancelled_periodically(cancel, checked) {
            cancelled = true;
            return std::ops::ControlFlow::Break(());
        }
        if !repeated {
            matched = ascii::contains_ci(name, term_lower);
        }
        if !delta.tombstones.get(record) && matched {
            if arena.is_dir(record) {
                let span = arena.subtree(record);
                set.push_span(span.start, span.end);
            } else {
                set.push_point(arena.dfs_position(record));
            }
        }
        std::ops::ControlFlow::Continue(())
    };
    if filtered {
        candidate_blocks.sort_unstable();
        candidate_blocks.dedup();
        for &block in candidate_blocks.iter() {
            let start = block * crate::trigram::TRIGRAM_BLOCK as u32;
            let end = start.saturating_add(crate::trigram::TRIGRAM_BLOCK as u32);
            arena.for_each_name_in_with_repeats(start..end, &mut visit);
            if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
                return false;
            }
        }
    } else {
        arena.for_each_name_with_repeats(&mut visit);
    }
    if cancelled || cancel.is_some_and(|cancel| cancel.is_cancelled()) {
        return false;
    }
    #[cfg(test)]
    PATH_TERM_SCAN_NS.fetch_add(
        scan_started.elapsed().as_nanos() as u64,
        AtomicOrdering::Relaxed,
    );
    #[cfg(test)]
    let coalesce_started = std::time::Instant::now();
    set.coalesce();
    #[cfg(test)]
    PATH_TERM_COALESCE_NS.fetch_add(
        coalesce_started.elapsed().as_nanos() as u64,
        AtomicOrdering::Relaxed,
    );
    true
}

fn path_match_mask(automaton: &aho_corasick::AhoCorasick, name: &[u8]) -> u16 {
    automaton
        .find_overlapping_iter(name)
        .fold(0u16, |mask, found| {
            mask | (1u16 << found.pattern().as_usize())
        })
}

// Fixed query-local budgets, independent of the number of indexed directories.
pub(super) const PATH_VERIFY_CANDIDATES: u64 = 1_024;
pub(super) const PATH_VERIFY_STEPS: usize = 16_384;
const PATH_VERIFY_MEMO_SLOTS: usize = 2_048;
pub(super) const PATH_VERIFY_DEPTH: usize = 512;
const PATH_VERIFY_NAME_BYTES: usize = 1024 * 1024;

/// Verify a small seed set without constructing the other terms' interval
/// sets. `false` means abandon this partial set and use the exhaustive fold;
/// `Err` means cancellation. Cache only complete directory-ancestor masks.
/// Direct-mapped cache collisions replace entries, never merge identities.
#[allow(clippy::too_many_arguments)]
pub(super) fn verify_path_seed(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    seed: &crate::intervals::IntervalSet,
    automaton: &aho_corasick::AhoCorasick,
    full_mask: u16,
    output: &mut crate::intervals::IntervalSet,
    name: &mut Vec<u8>,
    mut cancelled: impl FnMut() -> bool,
    max_steps: usize,
) -> Result<bool, ()> {
    output.clear();
    if seed.len_positions() > PATH_VERIFY_CANDIDATES {
        return Ok(false);
    }
    let mut memo = [(PARENT_NONE, 0u16); PATH_VERIFY_MEMO_SLOTS];
    let mut chain = [(PARENT_NONE, 0u16); PATH_VERIFY_DEPTH];
    let mut steps = 0;
    let mut bytes = 0usize;
    for position in seed.iter_positions() {
        let record = arena.dfs_record(position);
        if cancelled() {
            return Err(());
        }
        if delta.tombstones.get(record) {
            continue;
        }
        let mut current = record;
        let mut depth = 0;
        let mut own = 0;
        let mut inherited = loop {
            if cancelled() {
                return Err(());
            }
            if steps >= max_steps || depth == chain.len() {
                return Ok(false);
            }
            steps += 1;
            if current == PARENT_NONE {
                break 0;
            }
            let slot = current as usize % memo.len();
            // The leaf's own name contributes even when it is not a directory.
            if current != record && memo[slot].0 == current {
                break memo[slot].1;
            }
            arena.name_into(current, name);
            bytes = bytes.saturating_add(name.len());
            if bytes > PATH_VERIFY_NAME_BYTES {
                return Ok(false);
            }
            let mask = if !delta.tombstones.get(current) {
                path_match_mask(automaton, name)
            } else {
                0
            };
            if current == record {
                own = mask;
            }
            chain[depth] = (current, if arena.is_dir(current) { mask } else { 0 });
            depth += 1;
            current = arena.tree_parent(current);
        };
        for &(ancestor, mask) in chain[..depth].iter().rev() {
            inherited |= mask;
            if arena.is_dir(ancestor) {
                memo[ancestor as usize % PATH_VERIFY_MEMO_SLOTS] = (ancestor, inherited);
            }
        }
        if own | inherited == full_mask {
            output.push_ordered_point(position);
        }
    }
    if cancelled() {
        Err(())
    } else {
        Ok(true)
    }
}

#[allow(clippy::too_many_arguments)]
fn retain_dense_path_hits_parallel(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    runs: &crate::intervals::IntervalSet,
    automaton: &aho_corasick::AhoCorasick,
    term_count: usize,
    filter: &QueryFilter,
    order: Order,
    limit: usize,
    cancel: Option<Cancellation>,
    threads: usize,
) -> Option<BinaryHeap<u64>> {
    let matches = path_record_bitmap(arena, runs, cancel)?;
    let matches = &matches;
    let ranges: Vec<_> = crate::query::bucket_shards(arena.len(), threads).collect();
    let heaps: Vec<_> = ranges
        .iter()
        .map(|_| BinaryHeap::with_capacity(limit.min(4096)))
        .collect();
    let shards: Vec<BinaryHeap<u64>> = std::thread::scope(|scope| {
        ranges
            .into_iter()
            .zip(heaps)
            .map(|((start, end), mut heap)| {
                std::thread::Builder::new()
                    .stack_size(64 * 1024)
                    .spawn_scoped(scope, move || {
                        let mut checked = 0u32;
                        let mut own_mask = None;
                        arena.for_each_name_in_with_repeats(
                            start..end,
                            |record, name, repeated| {
                                if !repeated {
                                    own_mask = None;
                                }
                                if is_cancelled_periodically(cancel, &mut checked) {
                                    return std::ops::ControlFlow::Break(());
                                }
                                if delta.tombstones.get(record)
                                    || matches[record as usize / 8] & (1 << (record % 8)) == 0
                                    || !record_matches_filter(arena, delta, record, name, filter)
                                {
                                    return std::ops::ControlFlow::Continue(());
                                }
                                let own = *own_mask
                                    .get_or_insert_with(|| path_match_mask(automaton, name));
                                let quality = (term_count as u32 - own.count_ones()) as u8;
                                rank::retain_hit(
                                    &mut heap,
                                    sort_key(
                                        order,
                                        arena,
                                        delta,
                                        record,
                                        quality,
                                        name.len() as u32,
                                    ),
                                    limit,
                                );
                                std::ops::ControlFlow::Continue(())
                            },
                        );
                        heap
                    })
                    .expect("dense path worker spawn failed")
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
        return None;
    }
    let mut merged = BinaryHeap::with_capacity(limit.min(4096));
    for shard in shards {
        for key in shard {
            rank::retain_hit(&mut merged, key, limit);
        }
    }
    Some(merged)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn search_path_terms_with_scratch(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    terms: &[String],
    filter: &QueryFilter,
    options: SearchOptions,
    scratch: &mut PathSearchScratch,
    use_filter: bool,
    cancel: Option<Cancellation>,
    threads: usize,
) -> Vec<Hit> {
    let SearchOptions { limit, order } = options;
    if terms.is_empty() || terms.len() > crate::terms::MAX_TERMS || limit == 0 {
        return Vec::new();
    }
    if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
        return Vec::new();
    }
    scratch.reset(terms.len(), limit);
    // The name-decode-and-rank step below is skipped entirely for orderings
    // that don't need it: `sort_key` only reads `quality`/`name_len` for
    // `Order::Relevance`.
    let needs_name = !order.needs_metadata() || !filter.extensions.is_empty();
    let automaton = match aho_corasick::AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .build(terms)
    {
        Ok(automaton) => automaton,
        Err(_) => return Vec::new(),
    };
    let mask_for = |name: &[u8]| path_match_mask(&automaton, name);
    // `contains_ci`/`candidate_blocks_into` only lowercase the haystack, so
    // terms are lowered once up front.
    let terms_lower: Vec<Vec<u8>> = terms
        .iter()
        .map(|term| term.as_bytes().to_ascii_lowercase())
        .collect();

    let full_mask = ((1u32 << terms.len()) - 1) as u16;
    let mut build_order = [0usize; crate::terms::MAX_TERMS];
    for (index, slot) in build_order[..terms.len()].iter_mut().enumerate() {
        *slot = index;
    }
    // Survival estimates order independent scans; they are never intersected
    // across terms. Delta additions still need all base term sets for their
    // ancestor checks, so retain the existing algorithm for those views.
    let can_plan = use_filter && terms.len() > 1 && delta.added.is_empty();
    if can_plan {
        let mut costs = [0usize; crate::terms::MAX_TERMS];
        for (index, term) in terms_lower.iter().enumerate() {
            if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
                return Vec::new();
            }
            costs[index] = if arena.candidate_blocks_into(
                term,
                &mut scratch.candidate_blocks,
                &mut scratch.candidate_bitmap,
                &mut scratch.trigram_hashes,
            ) {
                scratch.candidate_blocks.len()
            } else {
                arena.len().div_ceil(crate::trigram::TRIGRAM_BLOCK)
            };
        }
        build_order[..terms.len()].sort_by_key(|&index| costs[index]);
    }

    let PathSearchScratch {
        term_sets,
        candidate_blocks,
        candidate_bitmap,
        trigram_hashes,
        own_matches,
        fold_a,
        name,
        ..
    } = &mut *scratch;
    let mut checked: u32 = 0;
    let mut verified_seed = false;
    for (build_index, &term_index) in build_order[..terms.len()].iter().enumerate() {
        let term_lower = &terms_lower[term_index];
        let term_set = &mut term_sets[term_index];
        let completed = if terms.len() == 1 && term_lower.len() < 3 {
            build_short_single_term_set(
                arena,
                delta,
                term_lower,
                term_set,
                own_matches,
                cancel,
                threads,
            )
        } else {
            build_term_interval_set(
                arena,
                delta,
                term_lower,
                use_filter,
                term_set,
                candidate_blocks,
                candidate_bitmap,
                trigram_hashes,
                cancel,
                &mut checked,
            )
        };
        if !completed {
            return Vec::new();
        }
        if term_set.is_empty()
            && !delta.added.iter().any(|record| {
                record.live && crate::ascii::contains_ci(record.name.as_bytes(), term_lower)
            })
        {
            // No base record or live delta record can introduce this term.
            // Descendants only inherit terms from those two sources, so later
            // full-index term scans cannot possibly recover a result.
            return Vec::new();
        }
        if can_plan && build_index == 0 {
            match verify_path_seed(
                arena,
                delta,
                term_set,
                &automaton,
                full_mask,
                fold_a,
                name,
                || cancel.is_some_and(|cancel| cancel.is_cancelled()),
                PATH_VERIFY_STEPS,
            ) {
                Ok(true) => {
                    verified_seed = true;
                    break;
                }
                Ok(false) => {}
                Err(()) => return Vec::new(),
            }
        }
    }

    // Never intersect per-term trigram candidate blocks: different terms may
    // be satisfied by different ancestor records. The per-term IntervalSets
    // above are built from fully independent scans; only their *derived* DFS
    // position sets are intersected here.
    let mut term_order = [0usize; crate::terms::MAX_TERMS];
    for (index, slot) in term_order[..terms.len()].iter_mut().enumerate() {
        *slot = index;
    }
    term_order[..terms.len()].sort_unstable_by_key(|&index| {
        (
            scratch.term_sets[index].len_positions(),
            scratch.term_sets[index].runs().len(),
        )
    });
    let PathSearchScratch {
        term_sets,
        fold_a,
        fold_b,
        heap,
        name,
        ..
    } = scratch;
    let final_set: &crate::intervals::IntervalSet = if verified_seed {
        fold_a
    } else if terms.len() == 1 {
        &term_sets[0]
    } else if term_sets[term_order[0]].is_empty() {
        fold_a.clear();
        fold_a
    } else {
        *fold_a = term_sets[term_order[0]].clone();
        let mut current_in_a = true;
        for &term_index in &term_order[1..terms.len()] {
            if current_in_a {
                term_sets[term_index].intersect_into(fold_a, fold_b);
                current_in_a = false;
            } else {
                term_sets[term_index].intersect_into(fold_b, fold_a);
                current_in_a = true;
            }
            let now_empty = if current_in_a {
                fold_a.is_empty()
            } else {
                fold_b.is_empty()
            };
            if now_empty {
                break;
            }
        }
        if current_in_a {
            fold_a
        } else {
            fold_b
        }
    };

    #[cfg(test)]
    let enumerate_started = std::time::Instant::now();
    let dense_parallel =
        needs_name && threads > 1 && final_set.len_positions() >= (arena.len() as u64).div_ceil(4);
    if dense_parallel {
        let Some(parallel_heap) = retain_dense_path_hits_parallel(
            arena,
            delta,
            final_set,
            &automaton,
            terms.len(),
            filter,
            order,
            limit,
            cancel,
            threads,
        ) else {
            return Vec::new();
        };
        *heap = parallel_heap;
    } else if needs_name && final_set.len_positions() > SPARSE_PATH_HITS {
        let mut own_mask = None;
        let completed = visit_path_candidate_names(
            arena,
            final_set,
            name,
            cancel,
            |record, record_name, repeated| {
                if !repeated {
                    own_mask = None;
                }
                if !delta.tombstones.get(record)
                    && record_matches_filter(arena, delta, record, record_name, filter)
                {
                    let mask = *own_mask.get_or_insert_with(|| mask_for(record_name));
                    rank::retain_hit(
                        heap,
                        sort_key(
                            order,
                            arena,
                            delta,
                            record,
                            (terms.len() as u32 - mask.count_ones()) as u8,
                            record_name.len() as u32,
                        ),
                        limit,
                    );
                }
            },
        );
        if !completed {
            return Vec::new();
        }
    } else {
        let mut checked: u32 = 0;
        for &(start, end) in final_set.runs() {
            for position in start..end {
                if is_cancelled_periodically(cancel, &mut checked) {
                    return Vec::new();
                }
                let record = arena.dfs_record(position);
                if delta.tombstones.get(record) {
                    continue;
                }
                if needs_name {
                    arena.name_into(record, name);
                }
                if !record_matches_filter(arena, delta, record, name, filter) {
                    continue;
                }
                let (quality, name_len) = if !order.needs_metadata() {
                    let mask = mask_for(name);
                    (
                        (terms.len() as u32 - mask.count_ones()) as u8,
                        name.len() as u32,
                    )
                } else {
                    (0, 0)
                };
                rank::retain_hit(
                    heap,
                    sort_key(order, arena, delta, record, quality, name_len),
                    limit,
                );
            }
        }
    }
    #[cfg(test)]
    PATH_TERM_ENUMERATE_NS.fetch_add(
        enumerate_started.elapsed().as_nanos() as u64,
        AtomicOrdering::Relaxed,
    );

    // The delta overlay is bounded and independent of the base pass above —
    // it always runs, even when a base term set came up empty, since a
    // delta-added directory can never have a base-record descendant (base
    // parents are always base records) but can still satisfy every term
    // through its own name plus its delta-and-base ancestor chain.
    let base_len = arena.len() as u32;
    let mut checked_delta: u32 = 0;
    for (index, record) in delta.added.iter().enumerate() {
        if !record.live {
            continue;
        }
        if is_cancelled_periodically(cancel, &mut checked_delta) {
            return Vec::new();
        }
        let combined = base_len + index as u32;
        if !record_matches_filter(arena, delta, combined, record.name.as_bytes(), filter) {
            continue;
        }
        let own = mask_for(record.name.as_bytes());
        let mut inherited = 0u16;
        let mut current_parent = record.parent;
        for _ in 0..512 {
            match current_parent {
                ParentRef::Base(parent) => {
                    let position = arena.dfs_position(parent);
                    for (term_index, term_set) in term_sets[..terms.len()].iter().enumerate() {
                        if term_set.contains(position) {
                            inherited |= 1 << term_index;
                        }
                    }
                    break;
                }
                ParentRef::Delta(parent_index) => {
                    let Some(ancestor) = delta.added.get(parent_index as usize) else {
                        break;
                    };
                    if !ancestor.live || !ancestor.is_dir {
                        break;
                    }
                    inherited |= mask_for(ancestor.name.as_bytes());
                    current_parent = ancestor.parent;
                }
                ParentRef::None => break,
            }
        }
        if own | inherited == full_mask {
            let (quality, name_len) = if needs_name {
                (
                    (terms.len() as u32 - own.count_ones()) as u8,
                    record.name.len() as u32,
                )
            } else {
                (0, 0)
            };
            rank::retain_hit(
                heap,
                sort_key(order, arena, delta, combined, quality, name_len),
                limit,
            );
        }
    }

    drain_heap(arena, delta, heap)
}
