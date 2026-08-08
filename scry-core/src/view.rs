use std::sync::Arc;
use std::{cell::RefCell, collections::BinaryHeap};

use regex_automata::meta::Regex;
use regex_automata::util::syntax;

use crate::ascii;
use crate::cancel::Cancellation;
use crate::delta::{Delta, ParentRef};
use crate::metrics::QuerySpans;
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

    /// A corpus for the streaming top-k tests: varied mtimes/sizes (so
    /// `Recent`/`Largest` have something to order on), a tombstoned base
    /// record (so a streaming candidate must still be filtered out without
    /// polluting the heap), and a live delta addition (so the merge with
    /// delta-added records is exercised alongside the streamed base match).
    fn streaming_corpus() -> (tempfile::TempDir, IndexView) {
        let mut builder = crate::ArenaBuilder::default();
        let root = builder.push("C:", 0, true);
        for i in 0..300u32 {
            let mtime = 1_000 + i;
            let size = ((i % 17) as u64 + 1) * 4_096;
            let child = builder.push_bytes_with_metadata(
                format!("needle_{i:04}.dat").as_bytes(),
                mtime,
                false,
                None,
                size,
            );
            builder.set_parent(child, root);
        }
        let (arena, _) = builder.build();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("streaming.rkyv");
        crate::store::save(&arena, &path).unwrap();
        let base = Arc::new(ArenaStore::open(&path).unwrap());
        let mut view = IndexView::new(base);
        let tombstoned = view.base.archived().prefix_range("needle_0005").start;
        let mut delta = Delta::new(view.base.archived().len());
        delta.tombstones.set(tombstoned);
        delta.added.push(DeltaRecord {
            name: "needle_delta.dat".into(),
            parent: ParentRef::Base(root),
            mtime_secs: 5_000,
            is_dir: false,
            size_bytes: 999_999,
            live: true,
        });
        view.delta = Arc::new(delta);
        (dir, view)
    }

    /// An independent collect-everything-then-rank reference: match every
    /// candidate with [`crate::query::search_base`] (never bounding the
    /// intermediate set), then apply the exact same key and tombstone rules
    /// the streaming path uses. If the streamed heap ever diverged from this
    /// — an off-by-one in a shard boundary, a tombstone slipping through, a
    /// delta-added record dropped from the merge — this catches it.
    fn collect_then_rank(view: &IndexView, query: &Query, order: Order, limit: usize) -> Vec<Hit> {
        let arena = view.base.archived();
        let delta = &view.delta;
        let mut heap = BinaryHeap::new();
        let needs_name = !order.needs_metadata();
        let mut name = Vec::new();
        for index in crate::query::search_base(arena, query, usize::MAX) {
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
        drain_heap(arena, delta, &mut heap)
    }

    /// The streamed per-thread-heap path (`search_ranked_streaming`) must
    /// return exactly what collecting every match first and ranking
    /// afterward would, for every ordering and both query shapes it handles.
    #[test]
    fn streaming_topk_matches_collect_then_rank() {
        let (_dir, view) = streaming_corpus();
        let queries = [
            Query::Substring("needle".into()),
            Query::Substring("0042".into()),
            Query::Regex(r"^needle_00.*\.dat$".into()),
        ];
        for query in &queries {
            for order in [Order::Relevance, Order::Recent, Order::Largest] {
                for limit in [1usize, 5, 1_000] {
                    let options = SearchOptions::ordered(limit, order);
                    let actual = view.search_hits(query, options);
                    let expected = collect_then_rank(&view, query, order, limit);
                    assert_eq!(
                        actual, expected,
                        "{query:?}, order={order:?}, limit={limit}"
                    );
                }
            }
        }
    }

    /// A cancelled streamed search must come back empty, never a partial
    /// heap from whichever shard happened to notice first.
    #[test]
    fn cancellation_yields_empty_not_partial() {
        let (_dir, view) = streaming_corpus();
        let generation = std::sync::atomic::AtomicU64::new(1);
        let cancel = Cancellation::new(&generation, 0); // already stale
        for threads in [1, 4] {
            let hits = view.search_hits_cancellable(
                &Query::Substring("needle".into()),
                SearchOptions::ordered(50, Order::Relevance),
                Some(cancel),
                threads,
            );
            assert!(hits.is_empty(), "threads={threads}");
        }
    }

    /// Splitting the unfiltered scan across threads must not change which
    /// records win the ranking — only how the work of finding and ranking
    /// them is divided. A single-character substring has no trigram filter,
    /// so it always takes the full-scan path this exercises.
    #[test]
    fn parallel_and_single_threaded_agree() {
        let (_dir, view) = streaming_corpus();
        let query = Query::Substring("e".into());
        for order in [Order::Relevance, Order::Recent, Order::Largest] {
            for limit in [3usize, 50] {
                let options = SearchOptions::ordered(limit, order);
                let single = view.search_hits_cancellable(&query, options, None, 1);
                for threads in [2, 4, 8] {
                    let parallel = view.search_hits_cancellable(&query, options, None, threads);
                    assert_eq!(
                        single, parallel,
                        "order={order:?}, limit={limit}, threads={threads}"
                    );
                }
            }
        }
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

    /// Direct, brute-force-checked test of the rank arithmetic `compact`
    /// uses to translate a base record's old index into its post-compaction
    /// index (`old - tombstones_before(old)`), independent of the rest of
    /// compaction — this is the one piece of index math a full-index bug
    /// would be easy to miss inside an otherwise-passing end-to-end test.
    #[test]
    fn tombstone_rank_formula_matches_bruteforce() {
        let n = 500usize;
        let mut tombstones = Delta::new(n).tombstones;
        let mut state = 0x1234_5678u32;
        for i in 0..n as u32 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if state.is_multiple_of(5) {
                tombstones.set(i);
            }
        }
        // Boundary cases: first and last position, and (via the modulus
        // above) runs of consecutive tombstones.
        tombstones.set(0);
        tombstones.set(n as u32 - 1);

        let superblocks = crate::bitvec::build_superblocks(tombstones.as_bytes());
        let rank = crate::bitvec::RankSelect::new(tombstones.as_bytes(), &superblocks);

        for old in 0..n as u32 {
            let bruteforce_before = (0..old).filter(|&i| tombstones.get(i)).count() as u32;
            assert_eq!(
                rank.rank1(old as usize) as u32,
                bruteforce_before,
                "old={old}"
            );
            if !tombstones.get(old) {
                assert_eq!(
                    old - rank.rank1(old as usize) as u32,
                    old - bruteforce_before
                );
            }
        }
    }

    /// End-to-end check that FRNs survive compaction under tombstones: every
    /// FRN belonging to a still-live record must resolve, via the compacted
    /// `.frn` sidecar, to a record whose path is unchanged. This exercises
    /// the sorted-by-index merge that replaced the old dense
    /// `old index -> Option<frn>` array — a bug there would silently drop or
    /// misattribute FRNs rather than fail to compile.
    #[test]
    fn compaction_preserves_frns_under_tombstones() {
        let mut builder = crate::ArenaBuilder::default();
        let root = builder.push_bytes_with_frn(b"C:", 0, true, 1);
        let mut frn = 2u64;
        // FRNs of leaf pushes, not their provisional builder indices — `build()`
        // sorts by name and reassigns indices, so a provisional index doesn't
        // identify the same record once the arena is saved and reopened.
        let mut leaf_frns = Vec::new();
        let mut dirs = vec![root];
        for level in 0..2 {
            let mut next_dirs = Vec::new();
            for &parent in &dirs {
                for i in 0..5 {
                    let dir_name = format!("d{level}_{i}");
                    let child = builder.push_bytes_with_frn(dir_name.as_bytes(), 0, true, frn);
                    frn += 1;
                    builder.set_parent(child, parent);
                    next_dirs.push(child);

                    let leaf_name = format!("f{level}_{i}.txt");
                    let leaf_frn = frn;
                    let leaf = builder.push_bytes_with_frn(leaf_name.as_bytes(), 0, false, frn);
                    frn += 1;
                    builder.set_parent(leaf, child);
                    leaf_frns.push(leaf_frn);
                }
            }
            dirs = next_dirs;
        }
        let (arena, mut frns) = builder.build();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("view.rkyv");
        crate::store::save_with_sidecar(&arena, &mut frns, &path, |_| {}, |_| {}).unwrap();
        let base = Arc::new(ArenaStore::open(&path).unwrap());
        let mut view = IndexView::new(base);
        let n = view.base.archived().len() as u32;
        let base_frn_map = view.base.frn_map.as_ref().unwrap();

        let mut delta = Delta::new(n as usize);
        // Only tombstone leaves (no live descendants), so every surviving
        // record's full path is unaffected by the tombstones.
        for (i, &leaf_frn) in leaf_frns.iter().enumerate() {
            if i % 3 == 0 {
                let leaf = base_frn_map.lookup(leaf_frn).unwrap();
                delta.tombstones.set(leaf);
            }
        }
        delta.added.push(DeltaRecord {
            name: "delta_root_child".into(),
            parent: ParentRef::Base(root),
            mtime_secs: 0,
            is_dir: true,
            size_bytes: 0,
            live: true,
        });
        delta.added.push(DeltaRecord {
            name: "delta_grandchild".into(),
            parent: ParentRef::Delta(0),
            mtime_secs: 0,
            is_dir: false,
            size_bytes: 0,
            live: true,
        });
        view.delta = Arc::new(delta);

        let archived = view.base.archived();
        let by_index: std::collections::HashMap<u32, u64> = view
            .base
            .frn_map
            .as_ref()
            .unwrap()
            .iter()
            .map(|entry| (entry.index, entry.frn))
            .collect();
        let mut expected_paths_by_frn: Vec<(u64, String)> = Vec::new();
        for old in 0..n {
            if view.delta.tombstones.get(old) {
                continue;
            }
            if let Some(&frn) = by_index.get(&old) {
                expected_paths_by_frn.push((frn, archived.full_path(old, '\\')));
            }
        }

        let (compacted_arena, mut compacted_frns) = view.compact();
        let out_path = dir.path().join("compacted.rkyv");
        crate::store::save_with_sidecar(
            &compacted_arena,
            &mut compacted_frns,
            &out_path,
            |_| {},
            |_| {},
        )
        .unwrap();
        let compacted_store = ArenaStore::open(&out_path).unwrap();
        let compacted_archived = compacted_store.archived();
        let compacted_frn_map = compacted_store.frn_map.as_ref().unwrap();

        assert_eq!(
            compacted_archived.len(),
            expected_paths_by_frn.len() + 2,
            "live base count + 2 delta additions"
        );
        for (frn, expected_path) in &expected_paths_by_frn {
            let new_index = compacted_frn_map
                .lookup(*frn)
                .unwrap_or_else(|| panic!("frn {frn} missing after compaction"));
            let actual_path = compacted_archived.full_path(new_index, '\\');
            assert_eq!(&actual_path, expected_path);
        }
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
    fn matches_path_terms_is_case_insensitive_regardless_of_which_side_carries_the_case() {
        let (_dir, view) = nested_view();
        let mut name = Vec::new();
        let file = view.base.archived().len() as u32 - 2; // quarterly.pdf, per nested_view's push order
        assert!(view.matches_path_terms(file, &["REPORTS".into(), "QUARTERLY".into()], &mut name));
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

    /// A wider, adversarial corpus for the same full-path-scan oracle:
    /// directories 6+ levels deep, a cyclic parent pair, a self-parented
    /// record, and a delta overlay with both a tombstoned base file and live
    /// additions (including one nested under another delta addition). This
    /// is the reference every future path-term matching implementation is
    /// checked against — it reconstructs each live record's path
    /// independently of `search_path_terms` via `IndexView::path_of`, which
    /// walks parent pointers directly and is hop-capped, so it tolerates the
    /// cyclic/self parents below the same way `full_path`/`delta_path` do.
    /// A genuinely out-of-range ("dangling")
    /// parent index cannot appear here: `ArenaBuilder::build` panics on one
    /// (it remaps parents through a rank table sized to the record count),
    /// so the on-disk format guarantees every parent is `< n` or
    /// `PARENT_NONE` by construction. `dfs::build`'s own tests cover
    /// genuinely dangling parents at the raw-column level, below
    /// `ArenaBuilder`.
    fn adversarial_path_term_view() -> (tempfile::TempDir, IndexView) {
        let mut builder = crate::ArenaBuilder::default();
        let root = builder.push("C:", 0, true);
        let level1 = builder.push("Alpha", 0, true);
        builder.set_parent(level1, root);
        let level2 = builder.push("Bravo", 0, true);
        builder.set_parent(level2, level1);
        let level3 = builder.push("Charlie", 0, true);
        builder.set_parent(level3, level2);
        let level4 = builder.push("Delta", 0, true);
        builder.set_parent(level4, level3);
        let level5 = builder.push("Echo", 0, true);
        builder.set_parent(level5, level4);
        let level6 = builder.push("Foxtrot", 0, true);
        builder.set_parent(level6, level5);
        let leaf = builder.push("deepfile.txt", 0, false);
        builder.set_parent(leaf, level6);

        // A directory-only match: nothing under it has a distinguishing name.
        let dir_only = builder.push("dironlyzqxv", 0, true);
        builder.set_parent(dir_only, root);
        let dir_only_child = builder.push("plainchild.bin", 0, false);
        builder.set_parent(dir_only_child, dir_only);

        // A leaf-only match: the distinguishing name is only on the file.
        let leaf_only_dir = builder.push("plaindir", 0, true);
        builder.set_parent(leaf_only_dir, root);
        let leaf_only = builder.push("leafonlywkyp.bin", 0, false);
        builder.set_parent(leaf_only, leaf_only_dir);

        // A cyclic parent pair, structurally independent of the rest.
        let cycle_a = builder.push("CycleA", 0, true);
        let cycle_b = builder.push("CycleB", 0, true);
        builder.set_parent(cycle_a, cycle_b);
        builder.set_parent(cycle_b, cycle_a);
        let cycle_child = builder.push("cyclechild.txt", 0, false);
        builder.set_parent(cycle_child, cycle_a);

        // A self-parented record.
        let self_parented = builder.push("SelfParented", 0, true);
        builder.set_parent(self_parented, self_parented);

        let (arena, _) = builder.build();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("adversarial-path-terms.rkyv");
        crate::store::save(&arena, &path).unwrap();
        let base = Arc::new(ArenaStore::open(&path).unwrap());
        let mut view = IndexView::new(base);

        let tombstoned = view.base.archived().prefix_range("plainchild").start;
        // `ArenaBuilder::build` name-sorts records, so pre-build indices like
        // `root`/`level3` no longer identify the same record; look their
        // post-build positions up by name instead (both names are unique in
        // this corpus).
        let root_post_build = view.base.archived().prefix_range("c:").start;
        let level3_post_build = view.base.archived().prefix_range("charlie").start;
        let base_len = view.base.archived().len();
        let mut delta = Delta::new(base_len);
        delta.tombstones.set(tombstoned);
        delta.added.push(DeltaRecord {
            name: "deltadirqzxv".into(),
            parent: ParentRef::Base(root_post_build),
            mtime_secs: 0,
            is_dir: true,
            size_bytes: 0,
            live: true,
        });
        let delta_dir_index = (delta.added.len() - 1) as u32;
        delta.added.push(DeltaRecord {
            name: "deltaleafwkyp.txt".into(),
            parent: ParentRef::Delta(delta_dir_index),
            mtime_secs: 0,
            is_dir: false,
            size_bytes: 0,
            live: true,
        });
        // A live delta addition parented directly under an existing base
        // directory, exercising the `ParentRef::Base` termination case
        // independently of the nested-under-delta case above.
        delta.added.push(DeltaRecord {
            name: "deltaunderbasewkyp.txt".into(),
            parent: ParentRef::Base(level3_post_build),
            mtime_secs: 0,
            is_dir: false,
            size_bytes: 0,
            live: true,
        });
        view.delta = Arc::new(delta);
        (dir, view)
    }

    #[test]
    fn path_terms_match_bruteforce_oracle() {
        let (_dir, view) = adversarial_path_term_view();
        let base_len = view.base.archived().len() as u32;
        let total = base_len + view.delta.added.len() as u32;

        let sixteen_terms: Vec<String> = (0..16).map(|i| format!("t{i}zqxv")).collect();
        let cases: Vec<Vec<String>> = vec![
            vec!["alpha".to_string()],
            vec!["foxtrot".to_string(), "deepfile".to_string()],
            vec!["dironlyzqxv".to_string()],
            vec!["leafonlywkyp".to_string()],
            vec!["plaindir".to_string(), "leafonlywkyp".to_string()],
            vec!["nothing_matches_this_zqxv".to_string()],
            vec!["cyclea".to_string(), "cyclechild".to_string()],
            vec!["selfparented".to_string()],
            vec!["deltadirqzxv".to_string(), "deltaleafwkyp".to_string()],
            vec!["deltaunderbasewkyp".to_string()],
            vec!["c:".to_string(), "plainchild".to_string()], // tombstoned leaf, must not match
            vec![
                "alpha".to_string(),
                "bravo".to_string(),
                "charlie".to_string(),
                "delta".to_string(),
                "echo".to_string(),
                "foxtrot".to_string(),
                "deepfile".to_string(),
                "c:".to_string(),
            ],
            sixteen_terms,
        ];

        for terms in cases {
            let mut expected = Vec::new();
            for record in 0..total {
                let live = if record < base_len {
                    !view.delta.tombstones.get(record)
                } else {
                    view.delta.added[(record - base_len) as usize].live
                };
                if !live {
                    continue;
                }
                let path = view.path_of(record);
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
            &terms,
            SearchOptions::new(usize::MAX),
            &mut PathSearchScratch::default(),
            true,
            None,
        );
        let full = search_path_terms_with_scratch(
            view.base.archived(),
            &view.delta,
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
            &["definitely_absent_qzxv".to_owned()],
            SearchOptions::new(10),
            &mut scratch,
            true,
            None,
        );
        assert!(results.is_empty());
        // A single-term query leaves exactly one term set behind, sized to
        // the smaller index rather than any leftover capacity from the
        // larger one.
        assert_eq!(scratch.term_sets.len(), 1);
        assert!(scratch.term_sets[0].is_empty());
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
    /// full path. The hop cap matches [`ArchivedArena::full_path`].
    pub fn matches_path_terms(&self, record: u32, terms: &[String], name: &mut Vec<u8>) -> bool {
        if terms.is_empty() || terms.len() > crate::terms::MAX_TERMS {
            return false;
        }
        let expected = (1u16 << terms.len()) - 1;
        // `contains_ci` only lowercases the haystack; the needle must already
        // be lowercase, so terms are lowered once up front rather than per
        // ancestor hop.
        let terms_lower: Vec<Vec<u8>> = terms
            .iter()
            .map(|term| term.as_bytes().to_ascii_lowercase())
            .collect();
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

        // Rank query over the tombstone bitset in place of a dense
        // `old index -> new index` array: compaction only removes base
        // records (it never reorders the survivors relative to each other),
        // so a live base record's new index is just its old index minus the
        // number of tombstones before it. `tomb_superblocks` costs ~4 bytes
        // per 256 records (~31 KB at 2M records) versus the 16 MB a
        // `Vec<Option<u32>>` over every base record cost.
        let tomb_superblocks = crate::bitvec::build_superblocks(self.delta.tombstones.as_bytes());
        let tomb_rank =
            crate::bitvec::RankSelect::new(self.delta.tombstones.as_bytes(), &tomb_superblocks);
        let base_new_index = |old: u32| -> Option<u32> {
            if self.delta.tombstones.get(old) {
                None
            } else {
                Some(old - tomb_rank.rank1(old as usize) as u32)
            }
        };

        let mut frns_by_delta = vec![None; self.delta.added.len()];
        for (&frn, &index) in &self.delta.added_frns {
            frns_by_delta[index as usize] = Some(frn);
        }

        let live_base = arena.len() - self.delta.tombstones.count_ones() as usize;
        let live_delta = self.delta.live_added().count();
        let mut builder = ArenaBuilder::with_capacity(live_base + live_delta, arena.names.len());
        let mut delta_indices = vec![None; self.delta.added.len()];
        let mut name = Vec::new();

        {
            // Sorted-by-index view of the FRN sidecar (stored FRN-sorted, for
            // `FrnMap::lookup`), consumed by one monotonic pass alongside the
            // old-index push loop below. Scoped to this block so it drops
            // once the loop finishes instead of surviving the rest of
            // compaction, unlike the dense `old index -> Option<frn>` array
            // it replaces.
            let mut by_index: Vec<FrnEntry> = match &self.base.frn_map {
                Some(map) => map.iter().collect(),
                None => Vec::new(),
            };
            by_index.sort_unstable_by_key(|entry| entry.index);
            let mut cursor = 0usize;

            for old in 0..arena.len() as u32 {
                if self.delta.tombstones.get(old) {
                    continue;
                }
                while cursor < by_index.len() && by_index[cursor].index < old {
                    cursor += 1;
                }
                let frn = (cursor < by_index.len() && by_index[cursor].index == old)
                    .then(|| by_index[cursor].frn);
                arena.name_into(old, &mut name);
                builder.push_bytes_with_metadata(
                    &name,
                    arena.mtime(old),
                    arena.is_dir(old),
                    frn,
                    arena.size_bytes(old),
                );
            }
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
            let Some(new) = base_new_index(old) else {
                continue;
            };
            let parent = arena.parent(old);
            if parent != PARENT_NONE {
                if let Some(new_parent) = base_new_index(parent) {
                    builder.set_parent(new, new_parent);
                }
            }
        }
        for (old, record) in self.delta.live_added() {
            let new = delta_indices[old as usize].unwrap();
            let parent = match record.parent {
                ParentRef::Base(index) => base_new_index(index),
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
        search_path_terms(arena, delta, terms, options)
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
    // Reused across the whole batch: `path_of_into` clears and refills it
    // per hit, so its backing capacity only has to grow once instead of once
    // per hit.
    let mut buf = String::new();
    hits.iter()
        .map(|hit| materialize(arena, delta, hit, &mut buf))
        .collect()
}

#[derive(Default)]
struct PathSearchScratch {
    /// One coalesced [`crate::intervals::IntervalSet`] of DFS positions per
    /// term, built from independent per-term scans — never merged at the
    /// trigram-block level, since different terms may be satisfied by
    /// different ancestor records.
    term_sets: Vec<crate::intervals::IntervalSet>,
    fold_a: crate::intervals::IntervalSet,
    fold_b: crate::intervals::IntervalSet,
    candidate_blocks: Vec<u32>,
    candidate_bitmap: Vec<u8>,
    trigram_hashes: Vec<usize>,
    heap: BinaryHeap<u64>,
    name: Vec<u8>,
}

impl PathSearchScratch {
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
        self.heap.clear();
        self.name.clear();
        let target = limit.min(4096);
        if self.heap.capacity() < target {
            self.heap.reserve(target);
        }
    }
}

thread_local! {
    static PATH_SEARCH_SCRATCH: RefCell<PathSearchScratch> =
        RefCell::new(PathSearchScratch::default());
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
    // `match_quality` needs the decoded name, and so does the length; neither
    // is needed by an ordering that ranks on a column, so skip the decode.
    let needs_name = !order.needs_metadata();
    let mut heap = match query {
        Query::Substring(_) | Query::Regex(_) => {
            let match_started = spans.as_ref().map(|_| std::time::Instant::now());
            let (heap, seen) = crate::query::search_ranked_streaming(
                arena,
                query,
                limit,
                threads,
                cancel,
                spans.as_deref_mut(),
                |index, name| {
                    if delta.tombstones.get(index) {
                        return None;
                    }
                    let (quality, name_len) = if needs_name {
                        (match_quality(query, name), name.len() as u32)
                    } else {
                        (0, 0)
                    };
                    Some(sort_key(order, arena, delta, index, quality, name_len))
                },
            );
            if let (Some(spans), Some(started)) = (spans.as_deref_mut(), match_started) {
                spans.match_ns = spans
                    .match_ns
                    .saturating_add(started.elapsed().as_nanos() as u64);
                spans.candidates = spans.candidates.saturating_add(seen);
            }
            heap
        }
        _ => {
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
            heap
        }
    };
    let rank_started = spans.as_ref().map(|_| std::time::Instant::now());

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
    if let (Some(spans), Some(started)) = (spans, rank_started) {
        spans.rank_ns = spans
            .rank_ns
            .saturating_add(started.elapsed().as_nanos() as u64);
    }
    hits
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

pub fn search_path_terms(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    terms: &[String],
    options: SearchOptions,
) -> Vec<Hit> {
    search_path_terms_cancellable(arena, delta, terms, options, None)
}

fn search_path_terms_cancellable(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    terms: &[String],
    options: SearchOptions,
    cancel: Option<Cancellation>,
) -> Vec<Hit> {
    PATH_SEARCH_SCRATCH.with(|scratch| {
        search_path_terms_with_scratch(
            arena,
            delta,
            terms,
            options,
            &mut scratch.borrow_mut(),
            true,
            cancel,
        )
    })
}

/// Scan the base arena for every live record whose own name contains
/// `term_lower`, folding its DFS interval into `set`: the whole subtree span
/// for a directory match (so descendants inherit the term through plain
/// interval containment), a single point for a file match. Returns `false`
/// if the scan was cancelled partway through, in which case `set` must be
/// discarded by the caller.
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
    let filtered = use_filter
        && arena.candidate_blocks_into(
            term_lower,
            candidate_blocks,
            candidate_bitmap,
            trigram_hashes,
        );
    if filtered {
        candidate_blocks.sort_unstable();
        candidate_blocks.dedup();
        for &block in candidate_blocks.iter() {
            let start = block * crate::trigram::TRIGRAM_BLOCK as u32;
            let end = start.saturating_add(crate::trigram::TRIGRAM_BLOCK as u32);
            arena.for_each_name_in(start..end, |record, name| {
                if is_cancelled_periodically(cancel, checked) {
                    return std::ops::ControlFlow::Break(());
                }
                if !delta.tombstones.get(record) && ascii::contains_ci(name, term_lower) {
                    if arena.is_dir(record) {
                        let span = arena.subtree(record);
                        set.push_span(span.start, span.end);
                    } else {
                        set.push_point(arena.dfs_position(record));
                    }
                }
                std::ops::ControlFlow::Continue(())
            });
            if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
                return false;
            }
        }
    } else {
        let mut cancelled = false;
        arena.for_each_name(|record, name| {
            if is_cancelled_periodically(cancel, checked) {
                cancelled = true;
                return std::ops::ControlFlow::Break(());
            }
            if !delta.tombstones.get(record) && ascii::contains_ci(name, term_lower) {
                if arena.is_dir(record) {
                    let span = arena.subtree(record);
                    set.push_span(span.start, span.end);
                } else {
                    set.push_point(arena.dfs_position(record));
                }
            }
            std::ops::ControlFlow::Continue(())
        });
        if cancelled || cancel.is_some_and(|cancel| cancel.is_cancelled()) {
            return false;
        }
    }
    set.coalesce();
    true
}

#[allow(clippy::too_many_arguments)]
fn search_path_terms_with_scratch(
    arena: &crate::ArchivedArena,
    delta: &Delta,
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
    scratch.reset(terms.len(), limit);
    // The name-decode-and-rank step below is skipped entirely for orderings
    // that don't need it: `sort_key` only reads `quality`/`name_len` for
    // `Order::Relevance`.
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
    // `contains_ci`/`candidate_blocks_into` only lowercase the haystack, so
    // terms are lowered once up front.
    let terms_lower: Vec<Vec<u8>> = terms
        .iter()
        .map(|term| term.as_bytes().to_ascii_lowercase())
        .collect();

    let PathSearchScratch {
        term_sets,
        candidate_blocks,
        candidate_bitmap,
        trigram_hashes,
        ..
    } = &mut *scratch;
    let mut checked: u32 = 0;
    for (term_lower, term_set) in terms_lower.iter().zip(term_sets.iter_mut()) {
        let completed = build_term_interval_set(
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
        );
        if !completed {
            return Vec::new();
        }
    }

    let full_mask = if terms.len() == 16 {
        u16::MAX
    } else {
        (1u16 << terms.len()) - 1
    };

    // Never intersect per-term trigram candidate blocks: different terms may
    // be satisfied by different ancestor records. The per-term IntervalSets
    // above are built from fully independent scans; only their *derived* DFS
    // position sets are intersected here.
    let any_term_empty = scratch.term_sets[..terms.len()]
        .iter()
        .any(|set| set.is_empty());
    let mut final_runs: Vec<(u32, u32)> = Vec::new();
    if !any_term_empty {
        scratch.fold_a = scratch.term_sets[0].clone();
        let mut current_in_a = true;
        for term_index in 1..terms.len() {
            if current_in_a {
                scratch.term_sets[term_index].intersect_into(&scratch.fold_a, &mut scratch.fold_b);
                current_in_a = false;
            } else {
                scratch.term_sets[term_index].intersect_into(&scratch.fold_b, &mut scratch.fold_a);
                current_in_a = true;
            }
            let now_empty = if current_in_a {
                scratch.fold_a.is_empty()
            } else {
                scratch.fold_b.is_empty()
            };
            if now_empty {
                break;
            }
        }
        let final_set = if current_in_a {
            &scratch.fold_a
        } else {
            &scratch.fold_b
        };
        final_runs.extend_from_slice(final_set.runs());
    }

    let mut checked: u32 = 0;
    for &(start, end) in &final_runs {
        for position in start..end {
            if is_cancelled_periodically(cancel, &mut checked) {
                return Vec::new();
            }
            let record = arena.dfs_record(position);
            if delta.tombstones.get(record) {
                continue;
            }
            let (quality, name_len) = if needs_name {
                arena.name_into(record, &mut scratch.name);
                let mask = mask_for(&scratch.name);
                (
                    (terms.len() as u32 - mask.count_ones()) as u8,
                    scratch.name.len() as u32,
                )
            } else {
                (0, 0)
            };
            rank::retain_hit(
                &mut scratch.heap,
                sort_key(order, arena, delta, record, quality, name_len),
                limit,
            );
        }
    }

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
        let own = mask_for(record.name.as_bytes());
        let mut inherited = 0u16;
        let mut current_parent = record.parent;
        for _ in 0..512 {
            match current_parent {
                ParentRef::Base(parent) => {
                    let position = arena.dfs_position(parent);
                    for (term_index, term_set) in
                        scratch.term_sets[..terms.len()].iter().enumerate()
                    {
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
                &mut scratch.heap,
                sort_key(order, arena, delta, combined, quality, name_len),
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
