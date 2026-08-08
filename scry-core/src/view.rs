use std::sync::Arc;
use std::{cell::RefCell, collections::BinaryHeap};

use regex_automata::meta::Regex;
use regex_automata::util::syntax;

use crate::ascii;
use crate::cancel::Cancellation;
use crate::delta::{Delta, ParentRef};
use crate::metrics::QuerySpans;
use crate::pathindex::{PathClosureScratch, PathIndex};
use crate::protocol::ResultEntry;
use crate::query::is_cancelled_periodically;
use crate::query::Query;
use crate::rank::{self, Order};
use crate::store::ArenaStore;
use crate::{Arena, ArenaBuilder, FrnEntry, PARENT_NONE};

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
    pub size: u64,
    pub mtime: u32,
    pub is_dir: bool,
}

/// Immutable base-and-overlay pair published through one atomic pointer.
pub struct IndexView {
    pub base: Arc<ArenaStore>,
    pub delta: Arc<Delta>,
    pub generation: u64,
    pub path_index: Arc<PathIndex>,
    pub journal_id: u64,
    pub next_usn: i64,
    pub volume_serial: u64,
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::delta::{DeltaRecord, ParentRef};

    /// The bounded top-k heap only bounds output; an unclamped caller-supplied
    /// limit would still let a one-character query size its intermediate
    /// state to the whole index.
    #[test]
    fn limit_is_clamped() {
        assert_eq!(SearchOptions::new(usize::MAX).limit, MAX_LIMIT);
        assert_eq!(
            SearchOptions::ordered(usize::MAX, Order::Recent).limit,
            MAX_LIMIT
        );
        assert_eq!(SearchOptions::new(50).limit, 50);
    }

    #[test]
    fn view_keeps_the_snapshot_cursor() {
        let mut builder = crate::ArenaBuilder::default();
        builder.set_snapshot_cursor(7, 11, 13);
        builder.push("C:", 0, true);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cursor.rkyv");
        crate::store::save(&builder.build().0, &path).unwrap();
        let view = IndexView::new(Arc::new(ArenaStore::open(&path).unwrap()));
        assert_eq!(
            (view.journal_id, view.next_usn, view.volume_serial),
            (7, 11, 13)
        );
    }

    fn base_view(count: usize) -> (tempfile::TempDir, IndexView) {
        let mut builder = crate::ArenaBuilder::default();
        let root = builder.push("C:", 0, true);
        for i in 0..count {
            let child = builder.push(&format!("match_{i:04}.txt"), 0, false);
            builder.set_parent(child, root);
        }
        let (arena, _) = builder.build();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("view.rkyv");
        crate::store::save(&arena, &path).unwrap();
        let base = Arc::new(ArenaStore::open(&path).unwrap());
        (dir, IndexView::new(base))
    }

    #[test]
    fn empty_delta_search_matches_base_search() {
        let (_dir, view) = base_view(5_000);
        for i in 0..100 {
            let query = Query::Substring(format!("{:02}", i));
            let expected: Vec<String> =
                crate::query::search_base(view.base.archived(), &query, usize::MAX)
                    .into_iter()
                    .map(|index| view.base.archived().full_path(index, '\\'))
                    .collect();
            let actual: Vec<String> = view
                .search(&query, usize::MAX)
                .into_iter()
                .map(|entry| entry.path)
                .collect();
            assert_eq!(actual, expected);
        }
    }

    /// Ordering is the only thing that changes between these searches: the
    /// same query, the same limit, the same records. If `Order` were being
    /// dropped anywhere between `SearchOptions` and the heap key, every list
    /// here would come back in relevance order and be identical.
    #[test]
    fn each_order_sorts_by_its_own_column() {
        let mut builder = crate::ArenaBuilder::default();
        let root = builder.push("C:", 0, true);
        // Name length is deliberately anti-correlated with both mtime and
        // size, so a relevance key leaking through would be visible.
        // Sizes are KiB multiples because the column stores KiB, so anything
        // else comes back rounded up and the assertion would be about the
        // rounding rather than the ordering.
        for (index, (mtime, size)) in [(300u32, 1_024u64), (100, 9_216), (200, 5_120)]
            .into_iter()
            .enumerate()
        {
            let child = builder.push_bytes_with_metadata(
                format!("match_{}", "x".repeat(index + 1)).as_bytes(),
                mtime,
                false,
                None,
                size,
            );
            builder.set_parent(child, root);
        }
        let (arena, _) = builder.build();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ordered.rkyv");
        crate::store::save(&arena, &path).unwrap();
        let view = IndexView::new(Arc::new(ArenaStore::open(&path).unwrap()));

        let query = Query::Prefix("match".into());
        let mtimes = |order| -> Vec<u32> {
            view.search_hits(&query, SearchOptions::ordered(10, order))
                .iter()
                .map(|hit| hit.mtime)
                .collect()
        };
        assert_eq!(mtimes(Order::Recent), [300, 200, 100]);
        assert_eq!(
            view.search_hits(&query, SearchOptions::ordered(10, Order::Largest))
                .iter()
                .map(|hit| hit.size)
                .collect::<Vec<_>>(),
            [9_216, 5_120, 1_024]
        );
        assert_eq!(mtimes(Order::Relevance), [300, 100, 200], "shortest first");

        // The bounded heap must keep the *best* two, not the first two seen.
        assert_eq!(
            view.search_hits(&query, SearchOptions::ordered(2, Order::Recent))
                .iter()
                .map(|hit| hit.mtime)
                .collect::<Vec<_>>(),
            [300, 200]
        );
    }

    #[test]
    fn limit_is_applied_across_both_layers() {
        let (_dir, mut view) = base_view(50);
        let root = view.base.archived().prefix_range("C:").start;
        let mut delta = Delta::new(view.base.archived().len());
        for i in 0..5 {
            delta.added.push(DeltaRecord {
                name: format!("match_delta_{i}.txt"),
                parent: ParentRef::Base(root),
                mtime_secs: 0,
                is_dir: false,
                size_bytes: 0,
                live: true,
            });
        }
        view.delta = Arc::new(delta);
        assert_eq!(view.search(&Query::Prefix("match".into()), 10).len(), 10);
        let all = view.search(&Query::Prefix("match".into()), 100);
        assert_eq!(all.len(), 55);
        assert_eq!(
            all.iter()
                .filter(|entry| entry.path.contains("delta"))
                .count(),
            5
        );
        assert_eq!(view.search(&Query::Substring("DELTA".into()), 100).len(), 5);
    }

    #[test]
    fn delta_child_path_uses_delta_parent() {
        let (_dir, mut view) = base_view(0);
        let root = view.base.archived().prefix_range("C:").start;
        let mut delta = Delta::new(view.base.archived().len());
        delta.added.push(DeltaRecord {
            name: "X".into(),
            parent: ParentRef::Base(root),
            mtime_secs: 0,
            is_dir: true,
            size_bytes: 0,
            live: true,
        });
        delta.added.push(DeltaRecord {
            name: "y".into(),
            parent: ParentRef::Delta(0),
            mtime_secs: 0,
            is_dir: false,
            size_bytes: 0,
            live: true,
        });
        view.delta = Arc::new(delta);
        assert!(view.delta_path(1).ends_with("X\\y"));
    }

    fn open_compacted(view: &IndexView) -> (tempfile::TempDir, IndexView) {
        let (arena, mut frns) = view.compact();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compacted.rkyv");
        crate::store::save_with_sidecar(&arena, &mut frns, &path, |_| {}, |_| {}).unwrap();
        let base = Arc::new(ArenaStore::open(&path).unwrap());
        (dir, IndexView::new(base))
    }

    #[test]
    fn compaction_preserves_query_results() {
        let (_dir, mut view) = base_view(200);
        let root = view.base.archived().prefix_range("C:").start;
        let deleted = view.base.archived().prefix_range("match_0042").start;
        let mut delta = Delta::new(view.base.archived().len());
        delta.tombstones.set(deleted);
        delta.added.push(DeltaRecord {
            name: "match_delta.txt".into(),
            parent: ParentRef::Base(root),
            mtime_secs: 0,
            is_dir: false,
            size_bytes: 0,
            live: true,
        });
        view.delta = Arc::new(delta);

        let before: Vec<Vec<String>> = (0..50)
            .map(|i| {
                let mut paths: Vec<_> = view
                    .search(&Query::Substring(format!("{:02}", i)), usize::MAX)
                    .into_iter()
                    .map(|entry| entry.path)
                    .collect();
                paths.sort();
                paths
            })
            .collect();
        let (_compacted_dir, compacted) = open_compacted(&view);
        for (i, expected) in before.into_iter().enumerate() {
            let mut actual: Vec<_> = compacted
                .search(&Query::Substring(format!("{:02}", i)), usize::MAX)
                .into_iter()
                .map(|entry| entry.path)
                .collect();
            actual.sort();
            assert_eq!(actual, expected);
        }
    }

    /// A query over an uncompacted delta must return exactly what the
    /// equivalent compacted arena returns, at every delta size this project
    /// expects to see in practice — including well past the 5% threshold
    /// that triggers compaction, so a regression there is caught before the
    /// compactor ever runs. Also records per-ratio latency (`eprintln`, not
    /// an assertion — delta-scan cost is quantified for plan 025, not gated
    /// here).
    #[test]
    fn delta_ratio_equivalence_and_latency() {
        const BASE: usize = 10_000;
        for percent in [0u32, 1, 5, 20] {
            let (_dir, mut view) = base_view(BASE);
            let root = view.base.archived().prefix_range("C:").start;
            let added = BASE * percent as usize / 100;
            let mut delta = Delta::new(view.base.archived().len());
            for i in 0..added {
                delta.added.push(DeltaRecord {
                    name: format!("match_delta_{i:04}.txt"),
                    parent: ParentRef::Base(root),
                    mtime_secs: 0,
                    is_dir: false,
                    size_bytes: 0,
                    live: true,
                });
            }
            view.delta = Arc::new(delta);

            let query = Query::Substring("match".into());
            let started = std::time::Instant::now();
            let mut actual: Vec<_> = view
                .search(&query, usize::MAX)
                .into_iter()
                .map(|entry| entry.path)
                .collect();
            let elapsed = started.elapsed();
            eprintln!("delta {percent:>2}% ({added} added): {elapsed:?}");

            let (_compacted_dir, compacted) = open_compacted(&view);
            let mut expected: Vec<_> = compacted
                .search(&query, usize::MAX)
                .into_iter()
                .map(|entry| entry.path)
                .collect();
            actual.sort();
            expected.sort();
            assert_eq!(actual, expected, "diverged at delta ratio {percent}%");
            assert_eq!(actual.len(), BASE + added);
        }
    }

    #[test]
    fn compaction_removes_tombstones_and_empty_is_identity() {
        let (_dir, mut view) = base_view(20);
        let original = view.len();
        let empty = view.compact().0;
        assert_eq!(empty.len(), original);

        let mut delta = Delta::new(view.base.archived().len());
        assert!(delta.tombstones.set(3));
        assert!(delta.tombstones.set(7));
        view.delta = Arc::new(delta);
        let compacted = view.compact().0;
        assert_eq!(compacted.len(), original - 2);
    }

    #[test]
    fn shared_overlay_search_matches_rpc_search() {
        let (_dir, mut view) = base_view(200);
        let root = view.base.archived().prefix_range("C:").start;
        let deleted = view.base.archived().prefix_range("match_0042").start;
        let mut delta = Delta::new(view.base.archived().len());
        assert!(delta.tombstones.set(deleted));
        delta.added.push(DeltaRecord {
            name: "match_delta.txt".into(),
            parent: ParentRef::Base(root),
            mtime_secs: 0,
            is_dir: false,
            size_bytes: 17,
            live: true,
        });
        view.delta = Arc::new(delta);
        let encoded = view.delta.encode_query_overlay();
        let decoded = Delta::decode_query_overlay(&encoded, view.base.archived().len()).unwrap();
        for query in [
            Query::Prefix("match".into()),
            Query::Substring("DELTA".into()),
            Query::wildcard("*.txt"),
        ] {
            assert_eq!(
                search_archived_with_delta(
                    view.base.archived(),
                    &decoded,
                    &query,
                    SearchOptions::new(100),
                ),
                view.search(&query, 100)
            );
        }
    }

    fn nested_view() -> (tempfile::TempDir, IndexView) {
        let mut builder = crate::ArenaBuilder::default();
        let root = builder.push("C:", 0, true);
        let projects = builder.push("Projects", 0, true);
        builder.set_parent(projects, root);
        let documents = builder.push("Documents", 0, true);
        builder.set_parent(documents, projects);
        let reports = builder.push("Reports", 0, true);
        builder.set_parent(reports, documents);
        let file = builder.push("quarterly.pdf", 0, false);
        builder.set_parent(file, reports);
        let unrelated = builder.push("notes.txt", 0, false);
        builder.set_parent(unrelated, root);
        let arena = builder.build().0;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested.rkyv");
        crate::store::save(&arena, &path).unwrap();
        let base = Arc::new(ArenaStore::open(&path).unwrap());
        (dir, IndexView::new(base))
    }

    #[test]
    fn path_terms_finds_ancestor_only_matches() {
        let (_dir, view) = nested_view();
        let results = view.search(
            &Query::PathTerms(vec!["projects".into(), "reports".into()]),
            50,
        );
        assert!(results
            .iter()
            .any(|entry| entry.path.ends_with("Reports\\quarterly.pdf")));
        assert!(results
            .iter()
            .all(|entry| !entry.path.ends_with("notes.txt")));
    }

    #[test]
    fn path_terms_match_descendants_of_an_absolute_path() {
        let mut builder = Arena::builder();
        let root = builder.push("C:", 0, true);
        let program_files = builder.push("Program Files", 0, true);
        let app = builder.push("app.exe", 0, false);
        let outside = builder.push("outside.txt", 0, false);
        builder.set_parent(program_files, root);
        builder.set_parent(app, program_files);
        builder.set_parent(outside, root);
        let arena = builder.build().0;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absolute-path.rkyv");
        crate::store::save(&arena, &path).unwrap();
        let view = IndexView::new(Arc::new(ArenaStore::open(&path).unwrap()));

        let terms = crate::terms::parse_terms(r"C:\\Program Files").unwrap();
        let results = view.search(&Query::PathTerms(terms), 50);
        assert!(results
            .iter()
            .any(|entry| entry.path.ends_with("Program Files\\app.exe")));
        assert!(results
            .iter()
            .all(|entry| !entry.path.ends_with("outside.txt")));
    }

    #[test]
    fn path_terms_matches_a_brute_force_full_path_scan() {
        let (_dir, view) = nested_view();
        for terms in [
            vec!["projects".to_string()],
            vec!["documents".to_string(), "pdf".to_string()],
            vec!["c:".to_string(), "notes".to_string()],
            vec!["missing".to_string()],
        ] {
            let mut expected = Vec::new();
            for record in 0..view.base.archived().len() as u32 {
                let path = view.base.archived().full_path(record, '\\');
                let lower = path.to_ascii_lowercase();
                if terms
                    .iter()
                    .all(|term| lower.contains(&term.to_ascii_lowercase()))
                {
                    expected.push(path);
                }
            }
            let mut actual: Vec<_> = view
                .search(&Query::PathTerms(terms.clone()), usize::MAX)
                .into_iter()
                .map(|entry| entry.path)
                .collect();
            expected.sort();
            actual.sort();
            assert_eq!(actual, expected, "{terms:?}");
        }
    }

    #[test]
    fn search_allocates_no_path_for_a_non_result() {
        let (_dir, view) = base_view(20_000);
        crate::arena::reset_full_path_calls();
        let results = view.search(&Query::Substring("match".into()), 10);
        assert_eq!(results.len(), 10);
        assert!(crate::arena::full_path_calls() <= 10);
    }

    #[test]
    fn query_spans_are_opt_in_and_capture_search_work() {
        let (_dir, view) = base_view(100);
        let query = Query::Substring("match".into());
        let hits =
            view.search_hits_cancellable_with_spans(&query, SearchOptions::new(10), None, 1, None);
        assert_eq!(hits.len(), 10);

        let mut spans = QuerySpans::default();
        let hits = view.search_hits_cancellable_with_spans(
            &query,
            SearchOptions::new(10),
            None,
            1,
            Some(&mut spans),
        );
        assert_eq!(hits.len(), 10);
        assert_eq!(spans.candidates, 100);
        assert_eq!(spans.blocks_scanned, spans.blocks_total);
    }

    #[test]
    fn path_term_block_union_matches_a_forced_full_scan() {
        let mut builder = crate::ArenaBuilder::default();
        let root = builder.push("000_root", 0, true);
        let ancestor = builder.push("aaa_ancestorqzxv", 0, true);
        builder.set_parent(ancestor, root);
        for index in 0..2_300 {
            let filler = builder.push(&format!("mmm_component_{index:04}.dll"), 0, false);
            builder.set_parent(filler, root);
        }
        let leaf = builder.push("zzz_leafwkyp.dll", 0, false);
        builder.set_parent(leaf, ancestor);

        let (arena, _) = builder.build();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("block-union.rkyv");
        crate::store::save(&arena, &path).unwrap();
        let base = Arc::new(ArenaStore::open(&path).unwrap());
        let view = IndexView::new(base);
        let terms = vec!["ancestorqzxv".to_owned(), "leafwkyp".to_owned()];

        let filtered = search_path_terms_with_scratch(
            view.base.archived(),
            &view.delta,
            &view.path_index,
            &terms,
            SearchOptions::new(usize::MAX),
            &mut PathSearchScratch::default(),
            true,
            None,
        );
        let full = search_path_terms_with_scratch(
            view.base.archived(),
            &view.delta,
            &view.path_index,
            &terms,
            SearchOptions::new(usize::MAX),
            &mut PathSearchScratch::default(),
            false,
            None,
        );

        assert_eq!(filtered, full);
        assert_eq!(filtered.len(), 1);
        assert!(view
            .path_of(filtered[0].record)
            .ends_with("aaa_ancestorqzxv\\zzz_leafwkyp.dll"));
    }

    #[test]
    fn path_term_scratch_clears_masks_across_different_indexes() {
        let (_large_dir, large) = base_view(2_000);
        let (_small_dir, small) = nested_view();
        let mut scratch = PathSearchScratch::default();

        let absent = search_path_terms_with_scratch(
            large.base.archived(),
            &large.delta,
            &large.path_index,
            &["match".to_owned()],
            SearchOptions::new(10),
            &mut scratch,
            true,
            None,
        );
        assert_eq!(absent.len(), 10);

        let results = search_path_terms_with_scratch(
            small.base.archived(),
            &small.delta,
            &small.path_index,
            &["definitely_absent_qzxv".to_owned()],
            SearchOptions::new(10),
            &mut scratch,
            true,
            None,
        );
        assert!(results.is_empty());
        assert!(scratch
            .touched_dirs
            .iter()
            .all(|&directory| directory < small.path_index.directory_count() as u32));
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
                let record =
                    builder.push(&format!("module_{group:03}_{file:06}_wkyp.dll"), 0, false);
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
                &view.path_index,
                &terms,
                SearchOptions::new(50),
                scratch,
                filtered,
                None,
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
}

impl IndexView {
    pub fn new(base: Arc<ArenaStore>) -> Self {
        let records = base.archived().len();
        let journal_id = base.archived().journal_id;
        let next_usn = base.archived().next_usn;
        let volume_serial = base.archived().volume_serial;
        let delta = Arc::new(Delta::new(records));
        let path_index = Arc::new(PathIndex::build(base.archived(), &delta));
        Self {
            base,
            delta,
            generation: fresh_generation(),
            path_index,
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
                &self.path_index,
                terms,
                options,
                cancel,
            );
            if let (Some(spans), Some(started)) = (spans, started) {
                spans.match_ns = spans
                    .match_ns
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
        path_of(self.base.archived(), &self.delta, record)
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
    /// full path. The hop cap matches [`ArchivedArena::full_path`].
    pub fn matches_path_terms(&self, record: u32, terms: &[String], name: &mut Vec<u8>) -> bool {
        if terms.is_empty() || terms.len() > crate::terms::MAX_TERMS {
            return false;
        }
        let expected = (1u16 << terms.len()) - 1;
        let mut matched = 0u16;
        let mut current = record;
        for _ in 0..512 {
            self.name_into(current, name);
            for (index, term) in terms.iter().enumerate() {
                if crate::ascii::contains_ci(name, term.as_bytes()) {
                    matched |= 1 << index;
                }
            }
            if matched == expected {
                return true;
            }
            let base_len = self.base.archived().len() as u32;
            if current < base_len {
                let parent = self.base.archived().parent(current);
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
        materialize(self.base.archived(), &self.delta, hit)
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

    pub fn delta_path(&self, mut index: u32) -> String {
        let mut parts = Vec::new();
        let mut base_parent = None;
        for _ in 0..512 {
            let record = &self.delta.added[index as usize];
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
            Some(parent) => format!("{}\\{suffix}", self.base.archived().full_path(parent, '\\')),
            None => suffix,
        }
    }

    /// Merge the overlay into a new base without re-enumerating the volume.
    pub fn compact(&self) -> (Arena, Vec<FrnEntry>) {
        let arena = self.base.archived();
        let mut frns_by_base = vec![None; arena.len()];
        if let Some(map) = &self.base.frn_map {
            for entry in map.iter() {
                if let Some(slot) = frns_by_base.get_mut(entry.index as usize) {
                    *slot = Some(entry.frn);
                }
            }
        }
        let mut frns_by_delta = vec![None; self.delta.added.len()];
        for (&frn, &index) in &self.delta.added_frns {
            frns_by_delta[index as usize] = Some(frn);
        }

        let live_base = arena.len() - self.delta.tombstones.count_ones() as usize;
        let live_delta = self.delta.live_added().count();
        let mut builder = ArenaBuilder::with_capacity(live_base + live_delta, arena.names.len());
        let mut base_indices = vec![None; arena.len()];
        let mut delta_indices = vec![None; self.delta.added.len()];
        let mut name = Vec::new();

        for old in 0..arena.len() as u32 {
            if self.delta.tombstones.get(old) {
                continue;
            }
            arena.name_into(old, &mut name);
            let new = match frns_by_base[old as usize] {
                Some(frn) => builder.push_bytes_with_metadata(
                    &name,
                    arena.mtime(old),
                    arena.is_dir(old),
                    Some(frn),
                    arena.size_bytes(old),
                ),
                None => builder.push_bytes_with_metadata(
                    &name,
                    arena.mtime(old),
                    arena.is_dir(old),
                    None,
                    arena.size_bytes(old),
                ),
            };
            base_indices[old as usize] = Some(new);
        }
        for (old, record) in self.delta.live_added() {
            let new = match frns_by_delta[old as usize] {
                Some(frn) => builder.push_bytes_with_metadata(
                    record.name.as_bytes(),
                    record.mtime_secs,
                    record.is_dir,
                    Some(frn),
                    record.size_bytes,
                ),
                None => builder.push_bytes_with_metadata(
                    record.name.as_bytes(),
                    record.mtime_secs,
                    record.is_dir,
                    None,
                    record.size_bytes,
                ),
            };
            delta_indices[old as usize] = Some(new);
        }

        for old in 0..arena.len() as u32 {
            let Some(new) = base_indices[old as usize] else {
                continue;
            };
            let parent = arena.parent(old);
            if parent != PARENT_NONE {
                if let Some(new_parent) = base_indices[parent as usize] {
                    builder.set_parent(new, new_parent);
                }
            }
        }
        for (old, record) in self.delta.live_added() {
            let new = delta_indices[old as usize].unwrap();
            let parent = match record.parent {
                ParentRef::Base(index) => base_indices[index as usize],
                ParentRef::Delta(index) => delta_indices[index as usize],
                ParentRef::None => None,
            };
            if let Some(parent) = parent {
                builder.set_parent(new, parent);
            }
        }
        builder.build()
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
    let hits = if let Query::PathTerms(terms) = query {
        let path_index = PathIndex::build(arena, delta);
        search_path_terms(arena, delta, &path_index, terms, options)
    } else {
        search_ranked(arena, delta, query, options)
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
    hits.iter()
        .map(|hit| materialize(arena, delta, hit))
        .collect()
}

#[derive(Default)]
struct PathSearchScratch {
    hits: Vec<(u32, u16)>,
    dir_mask: Vec<u16>,
    touched_dirs: Vec<u32>,
    closure: PathClosureScratch,
    parent_cache: ParentRankCache,
    relevant_parents: Vec<u8>,
    touched_parent_bytes: Vec<u32>,
    candidate_blocks: Vec<u32>,
    term_blocks: Vec<u32>,
    candidate_bitmap: Vec<u8>,
    trigram_hashes: Vec<usize>,
    heap: BinaryHeap<u64>,
    name: Vec<u8>,
}

impl PathSearchScratch {
    fn reset(&mut self, directories: usize, limit: usize) {
        for directory in self.touched_dirs.drain(..) {
            if let Some(mask) = self.dir_mask.get_mut(directory as usize) {
                *mask = 0;
            }
        }
        self.dir_mask.resize(directories, 0);
        self.dir_mask.truncate(directories);
        self.hits.clear();
        self.candidate_blocks.clear();
        self.term_blocks.clear();
        self.candidate_bitmap.clear();
        self.trigram_hashes.clear();
        self.heap.clear();
        self.name.clear();
        self.parent_cache.clear();
        for byte in self.touched_parent_bytes.drain(..) {
            if let Some(bits) = self.relevant_parents.get_mut(byte as usize) {
                *bits = 0;
            }
        }
        let target = limit.min(4096);
        if self.heap.capacity() < target {
            self.heap.reserve(target);
        }
    }

    fn merge_directory_mask(&mut self, directory: u32, bits: u16) {
        let mask = &mut self.dir_mask[directory as usize];
        if *mask == 0 {
            self.touched_dirs.push(directory);
        }
        *mask |= bits;
    }

    fn prepare_parent_filter(&mut self, records: usize) {
        self.relevant_parents.resize(records.div_ceil(8), 0);
        self.relevant_parents.truncate(records.div_ceil(8));
    }

    fn mark_relevant_parent(&mut self, record: u32) {
        let byte = record as usize / 8;
        let bit = 1 << (record % 8);
        if self.relevant_parents[byte] == 0 {
            self.touched_parent_bytes.push(byte as u32);
        }
        self.relevant_parents[byte] |= bit;
    }

    fn parent_is_relevant(&self, record: u32) -> bool {
        self.relevant_parents[record as usize / 8] & (1 << (record % 8)) != 0
    }
}

const PARENT_CACHE_SLOTS: usize = 4096;

#[derive(Default)]
struct ParentRankCache {
    keys: Vec<u32>,
    values: Vec<u32>,
}

impl ParentRankCache {
    fn clear(&mut self) {
        self.keys.resize(PARENT_CACHE_SLOTS, PARENT_NONE);
        self.keys.fill(PARENT_NONE);
        self.values.resize(PARENT_CACHE_SLOTS, PARENT_NONE);
    }

    fn dir_ord(&mut self, path_index: &PathIndex, parent: u32) -> Option<u32> {
        let slot = parent.wrapping_mul(2_654_435_761) as usize & (PARENT_CACHE_SLOTS - 1);
        if self.keys[slot] != parent {
            self.keys[slot] = parent;
            self.values[slot] = path_index.dir_ord(parent).unwrap_or(PARENT_NONE);
        }
        (self.values[slot] != PARENT_NONE).then_some(self.values[slot])
    }
}

thread_local! {
    static PATH_SEARCH_SCRATCH: RefCell<PathSearchScratch> =
        RefCell::new(PathSearchScratch::default());
}

/// Keep `key` if it belongs in the best `limit` seen so far.
///
/// The heap is a max-heap over keys that sort ascending-is-better, so its root
/// is the worst retained candidate and eviction is a peek and a swap. See
/// [`crate::rank`] for why a candidate is one integer and not a struct.
fn retain_hit(heap: &mut BinaryHeap<u64>, key: u64, limit: usize) {
    if limit == 0 {
        return;
    }
    if heap.len() < limit {
        heap.push(key);
    } else if heap.peek().is_some_and(|worst| key < *worst) {
        heap.pop();
        heap.push(key);
    }
}

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

/// The sort key for one candidate under `order`.
///
/// `quality` and `name_len` are already in hand at every call site; the cold
/// `mtimes`/`sizes` reads happen only for the orderings that need them, which
/// is the point of [`Order::needs_metadata`].
#[inline]
fn sort_key(
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
        Order::Largest => rank::largest_key(record_size_kib(arena, delta, record), record),
    }
}

/// Turn the heap's retained keys into hits, best first.
fn drain_heap(arena: &crate::ArchivedArena, delta: &Delta, heap: &mut BinaryHeap<u64>) -> Vec<Hit> {
    let mut hits = Vec::with_capacity(heap.len());
    while let Some(key) = heap.pop() {
        hits.push(hit_for(arena, delta, rank::key_record(key)));
    }
    // The heap yields worst-first; the caller wants best-first.
    hits.reverse();
    hits
}

fn hit_for(arena: &crate::ArchivedArena, delta: &Delta, record: u32) -> Hit {
    match record.checked_sub(arena.len() as u32) {
        None => Hit {
            record,
            size: arena.size_bytes(record),
            mtime: arena.mtime(record),
            is_dir: arena.is_dir(record),
        },
        Some(index) => {
            let added = &delta.added[index as usize];
            Hit {
                record,
                size: added.size_bytes,
                mtime: added.mtime_secs,
                is_dir: added.is_dir,
            }
        }
    }
}

fn match_quality(query: &Query, name: &[u8]) -> u8 {
    let pattern = match query {
        Query::Prefix(pattern) | Query::Substring(pattern) => pattern.as_bytes(),
        Query::Regex(_) => return 2,
        Query::PathTerms(_) => unreachable!(),
    };
    if ascii::cmp_ci(name, pattern).is_eq() {
        0
    } else if ascii::starts_with_ci(name, pattern) {
        1
    } else {
        2
    }
}

fn search_ranked(
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

fn search_ranked_cancellable_with_spans(
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
    let match_started = spans.as_ref().map(|_| std::time::Instant::now());
    let base_hits = crate::query::search_base_parallel_with_spans(
        arena,
        query,
        threads,
        cancel,
        spans.as_deref_mut(),
    );
    if let (Some(spans), Some(started)) = (spans.as_deref_mut(), match_started) {
        spans.match_ns = spans
            .match_ns
            .saturating_add(started.elapsed().as_nanos() as u64);
        spans.candidates = spans.candidates.saturating_add(base_hits.len() as u64);
    }
    let rank_started = spans.as_ref().map(|_| std::time::Instant::now());
    let mut heap = BinaryHeap::with_capacity(limit.min(4096));
    let mut name = Vec::new();
    // `match_quality` needs the decoded name, and so does the length; neither
    // is needed by an ordering that ranks on a column, so skip the decode.
    let needs_name = !order.needs_metadata();
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
        retain_hit(
            &mut heap,
            sort_key(order, arena, delta, index, quality, name_len),
            limit,
        );
    }

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
            Query::PathTerms(_) => unreachable!(),
        };
        if matched {
            let combined = arena.len() as u32 + index;
            retain_hit(
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
    if let (Some(spans), Some(started)) = (spans, rank_started) {
        spans.rank_ns = spans
            .rank_ns
            .saturating_add(started.elapsed().as_nanos() as u64);
    }
    hits
}

/// Reconstruct the path of one hit. Separated from matching and ranking
/// because it is the expensive part — see [`Hit`].
fn materialize(arena: &crate::ArchivedArena, delta: &Delta, hit: &Hit) -> ResultEntry {
    ResultEntry {
        path: path_of(arena, delta, hit.record),
        size: hit.size,
        mtime: hit.mtime,
        is_dir: hit.is_dir,
    }
}

fn path_of(arena: &crate::ArchivedArena, delta: &Delta, record: u32) -> String {
    match record.checked_sub(arena.len() as u32) {
        None => arena.full_path(record, '\\'),
        Some(index) => delta_path(arena, delta, index),
    }
}

pub fn search_path_terms(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    path_index: &PathIndex,
    terms: &[String],
    options: SearchOptions,
) -> Vec<Hit> {
    search_path_terms_cancellable(arena, delta, path_index, terms, options, None)
}

fn search_path_terms_cancellable(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    path_index: &PathIndex,
    terms: &[String],
    options: SearchOptions,
    cancel: Option<Cancellation>,
) -> Vec<Hit> {
    PATH_SEARCH_SCRATCH.with(|scratch| {
        search_path_terms_with_scratch(
            arena,
            delta,
            path_index,
            terms,
            options,
            &mut scratch.borrow_mut(),
            true,
            cancel,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn search_path_terms_with_scratch(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    path_index: &PathIndex,
    terms: &[String],
    options: SearchOptions,
    scratch: &mut PathSearchScratch,
    use_filter: bool,
    cancel: Option<Cancellation>,
) -> Vec<Hit> {
    let SearchOptions { limit, order } = options;
    if terms.is_empty() || terms.len() > crate::terms::MAX_TERMS || limit == 0 {
        return Vec::new();
    }
    if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
        return Vec::new();
    }
    scratch.reset(path_index.directory_count(), limit);
    let needs_name = !order.needs_metadata();
    let automaton = match aho_corasick::AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .build(terms)
    {
        Ok(automaton) => automaton,
        Err(_) => return Vec::new(),
    };
    let mask_for = |name: &[u8]| {
        automaton
            .find_overlapping_iter(name)
            .fold(0u16, |mask, found| {
                mask | (1u16 << found.pattern().as_usize())
            })
    };

    let mut filtered = use_filter;
    if filtered {
        for term in terms {
            if !arena.candidate_blocks_into(
                term.as_bytes(),
                &mut scratch.term_blocks,
                &mut scratch.candidate_bitmap,
                &mut scratch.trigram_hashes,
            ) {
                filtered = false;
                scratch.candidate_blocks.clear();
                break;
            }
            scratch
                .candidate_blocks
                .extend_from_slice(&scratch.term_blocks);
        }
    }
    let mut checked: u32 = 0;
    let mut cancelled = false;
    if filtered {
        scratch.candidate_blocks.sort_unstable();
        scratch.candidate_blocks.dedup();
        'blocks: for block_index in 0..scratch.candidate_blocks.len() {
            let block = scratch.candidate_blocks[block_index];
            let start = block * crate::trigram::TRIGRAM_BLOCK as u32;
            let end = start.saturating_add(crate::trigram::TRIGRAM_BLOCK as u32);
            arena.for_each_name_in(start..end, |record, name| {
                if is_cancelled_periodically(cancel, &mut checked) {
                    return std::ops::ControlFlow::Break(());
                }
                if !delta.tombstones.get(record) {
                    let mask = mask_for(name);
                    if mask != 0 {
                        scratch.hits.push((record, mask));
                        if let Some(directory) = path_index.dir_ord(record) {
                            scratch.merge_directory_mask(directory, mask);
                        }
                    }
                }
                std::ops::ControlFlow::Continue(())
            });
            if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
                cancelled = true;
                break 'blocks;
            }
        }
    } else {
        arena.for_each_name(|record, name| {
            if is_cancelled_periodically(cancel, &mut checked) {
                return std::ops::ControlFlow::Break(());
            }
            if !delta.tombstones.get(record) {
                let mask = mask_for(name);
                if mask != 0 {
                    scratch.hits.push((record, mask));
                    if let Some(directory) = path_index.dir_ord(record) {
                        scratch.merge_directory_mask(directory, mask);
                    }
                }
            }
            std::ops::ControlFlow::Continue(())
        });
        cancelled = cancel.is_some_and(|cancel| cancel.is_cancelled());
    }
    if cancelled {
        return Vec::new();
    }
    for (index, record) in delta.added.iter().enumerate() {
        if !record.live {
            continue;
        }
        let combined = arena.len() as u32 + index as u32;
        let mask = mask_for(record.name.as_bytes());
        if mask != 0 {
            scratch.hits.push((combined, mask));
            if let Some(directory) = path_index.dir_ord(combined) {
                scratch.merge_directory_mask(directory, mask);
            }
        }
    }
    path_index.closure_sparse(
        &mut scratch.dir_mask,
        &mut scratch.touched_dirs,
        &mut scratch.closure,
    );
    let filter_parents = scratch.touched_dirs.len() < path_index.directory_count().div_ceil(4);
    if filter_parents {
        scratch.prepare_parent_filter(path_index.records());
        for touched in 0..scratch.touched_dirs.len() {
            if let Some(record) = path_index.dir_record(scratch.touched_dirs[touched]) {
                scratch.mark_relevant_parent(record);
            }
        }
    }

    let full_mask = if terms.len() == 16 {
        u16::MAX
    } else {
        (1u16 << terms.len()) - 1
    };
    let mut hit_position = 0usize;
    let mut checked: u32 = 0;
    for record in 0..path_index.records() as u32 {
        if is_cancelled_periodically(cancel, &mut checked) {
            return Vec::new();
        }
        let live = if record < arena.len() as u32 {
            !delta.tombstones.get(record)
        } else {
            delta
                .added
                .get(record as usize - arena.len())
                .is_some_and(|record| record.live)
        };
        if !live {
            continue;
        }
        while scratch
            .hits
            .get(hit_position)
            .is_some_and(|hit| hit.0 < record)
        {
            hit_position += 1;
        }
        let own = scratch
            .hits
            .get(hit_position)
            .filter(|hit| hit.0 == record)
            .map_or(0, |hit| hit.1);
        let inherited = path_index
            .parent_record(arena, delta, record)
            .filter(|&parent| !filter_parents || scratch.parent_is_relevant(parent))
            .and_then(|parent| scratch.parent_cache.dir_ord(path_index, parent))
            .map_or(0, |directory| scratch.dir_mask[directory as usize]);
        if own | inherited == full_mask {
            // As in `search_ranked_cancellable`: the name is read only for the
            // ordering that ranks on it.
            let name_len = if needs_name {
                if record < arena.len() as u32 {
                    arena.name_into(record, &mut scratch.name);
                    scratch.name.len() as u32
                } else {
                    let index = record - arena.len() as u32;
                    delta.added[index as usize].name.len() as u32
                }
            } else {
                0
            };
            retain_hit(
                &mut scratch.heap,
                sort_key(
                    order,
                    arena,
                    delta,
                    record,
                    (terms.len() as u32 - own.count_ones()) as u8,
                    name_len,
                ),
                limit,
            );
        }
    }
    drain_heap(arena, delta, &mut scratch.heap)
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
