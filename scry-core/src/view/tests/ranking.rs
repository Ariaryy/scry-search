use super::*;

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
fn carried_rank_bits_do_not_grow_a_hit() {
    assert_eq!(std::mem::size_of::<Hit>(), 24);
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

#[test]
fn metadata_filters_run_before_result_retention() {
    let (_dir, view) = base_view(200);
    let files = crate::terms::parse_query("match type:file ext:txt", 0).unwrap();
    let file_hits = view.search_hits(&files, SearchOptions::new(3));
    assert_eq!(file_hits.len(), 3);
    assert!(file_hits.iter().all(|hit| !hit.is_dir));

    let directories = crate::terms::parse_query("type:dir", 0).unwrap();
    let directory_hits = view.search_hits(&directories, SearchOptions::new(10));
    assert_eq!(directory_hits.len(), 1);
    assert!(directory_hits[0].is_dir);
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
fn repeated_names_keep_record_specific_ranking_and_tombstones() {
    let (_dir, view) = streaming_corpus_with_repeats(true);
    for query in [
        Query::Substring("ne".into()),
        Query::Substring("needle".into()),
        Query::Substring("0005".into()),
        Query::Substring("absent".into()),
        Query::Regex(".*".into()),
        Query::Regex(r"^needle_000[15]\.dat$".into()),
    ] {
        for order in [Order::Relevance, Order::Recent, Order::Largest] {
            for limit in [0, 1, 5, 1000] {
                let expected = collect_then_rank(&view, &query, order, limit);
                for threads in [1, 2, 4, 8] {
                    assert_eq!(
                        view.search_hits_cancellable(
                            &query,
                            SearchOptions::ordered(limit, order),
                            None,
                            threads
                        ),
                        expected,
                        "{query:?}, {order:?}, limit={limit}, threads={threads}"
                    );
                }
            }
        }
    }
}

#[test]
fn repeated_candidate_names_reset_after_skipped_records() {
    let (_dir, view) = streaming_corpus_with_repeats(true);
    let arena = view.base.archived();
    for sparse in [false, true] {
        let records: Vec<_> = (0..arena.len() as u32)
            .filter(|&record| {
                if sparse {
                    [2, 87, 173].contains(&record)
                } else {
                    record % 5 == 3
                }
            })
            .collect();
        let mut runs = crate::intervals::IntervalSet::default();
        for &record in &records {
            runs.push_point(arena.dfs_position(record));
        }
        runs.coalesce();
        assert_eq!(
            use_path_record_batches(arena.len(), runs.len_positions()),
            sparse
        );
        let mut previous = None;
        let mut actual = Vec::new();
        assert!(visit_path_candidate_names(
            arena,
            &runs,
            &mut Vec::new(),
            None,
            |record, name, repeated| {
                if repeated {
                    assert_eq!(previous.as_deref(), Some(name));
                }
                previous = Some(name.to_vec());
                actual.push(record);
            }
        ));
        actual.sort_unstable();
        assert_eq!(actual, records);
    }
}

#[test]
fn largest_places_incomplete_directory_after_exact_file() {
    let mut builder = crate::ArenaBuilder::default();
    let root = builder.push("root", 0, true);
    let directory = builder.push("match_large_dir", 0, true);
    let unknown =
        builder.push_bytes_with_metadata_exact(b"unmeasured.bin", 0, false, None, 1_000_000, false);
    let exact =
        builder.push_bytes_with_metadata_exact(b"match_small.bin", 0, false, None, 1_024, true);
    builder.set_parent(directory, root);
    builder.set_parent(unknown, directory);
    builder.set_parent(exact, root);
    let (arena, _) = builder.build();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("largest-exactness.rkyv");
    crate::store::save(&arena, &path).unwrap();
    let view = IndexView::new(Arc::new(ArenaStore::open(&path).unwrap()));

    let hits = view.search_hits(
        &Query::Prefix("match".into()),
        SearchOptions::ordered(10, Order::Largest),
    );
    assert!(hits[0].size_exact);
    assert_eq!(hits[0].size, 1_024);
    assert!(!hits[1].size_exact);
    assert!(hits[1].size > hits[0].size, "lower bound is retained");
}

/// A corpus for the streaming top-k tests: varied mtimes/sizes (so
/// `Recent`/`Largest` have something to order on), a tombstoned base
/// record (so a streaming candidate must still be filtered out without
/// polluting the heap), and a live delta addition (so the merge with
/// delta-added records is exercised alongside the streamed base match).
fn streaming_corpus() -> (tempfile::TempDir, IndexView) {
    streaming_corpus_with_repeats(false)
}

fn streaming_corpus_with_repeats(repeated: bool) -> (tempfile::TempDir, IndexView) {
    let mut builder = crate::ArenaBuilder::default();
    let root = builder.push("C:", 0, true);
    for i in 0..300u32 {
        let mtime = 1_000 + i;
        let size = ((i % 17) as u64 + 1) * 4_096;
        let child = builder.push_bytes_with_metadata(
            format!("needle_{:04}.dat", if repeated { i % 7 } else { i }).as_bytes(),
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
        size_exact: true,
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
    for index in 0..arena.len() as u32 {
        arena.name_into(index, &mut name);
        let matched = match query {
            Query::Prefix(prefix) => ascii::starts_with_ci(&name, prefix.as_bytes()),
            Query::Substring(_) => {
                ascii::contains_ci(&name, substring_lower.as_ref().unwrap().as_bytes())
            }
            Query::Regex(_) => regex
                .as_ref()
                .is_some_and(|compiled| compiled.is_match(&name)),
            _ => unreachable!(),
        };
        if !matched {
            continue;
        }

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
    drain_heap(arena, delta, &mut heap)
}

/// The streamed per-thread-heap path (`search_ranked_streaming`) must
/// return exactly what collecting every match first and ranking
/// afterward would, for every ordering and both query shapes it handles.
#[test]
fn streaming_topk_matches_collect_then_rank() {
    let (_dir, view) = streaming_corpus();
    let queries = [
        Query::Prefix("needle".into()),
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
        for query in [
            Query::Substring("needle".into()),
            Query::PathTerms(vec!["e".into()]),
        ] {
            let hits = view.search_hits_cancellable(
                &query,
                SearchOptions::ordered(50, Order::Relevance),
                Some(cancel),
                threads,
            );
            assert!(hits.is_empty(), "query={query:?}, threads={threads}");
        }
    }
}

/// Splitting the unfiltered scan across threads must not change which
/// records win the ranking — only how the work of finding and ranking
/// them is divided. A single-character substring has no trigram filter,
/// so it always takes the full-scan path this exercises.
#[test]
fn parallel_and_single_threaded_agree() {
    let (_dir, view) = streaming_corpus();
    for query in [
        Query::Substring("e".into()),
        Query::PathTerms(vec!["e".into()]),
        Query::PathTerms(vec!["ne".into()]),
        Query::PathTerms(vec!["needle".into()]),
    ] {
        for order in [Order::Relevance, Order::Recent, Order::Largest] {
            for limit in [0usize, 1, 50] {
                let options = SearchOptions::ordered(limit, order);
                let single = view.search_hits_cancellable(&query, options, None, 1);
                for threads in [2, 4, 8] {
                    let parallel = view.search_hits_cancellable(&query, options, None, threads);
                    assert_eq!(
                        single, parallel,
                        "query={query:?}, order={order:?}, limit={limit}, threads={threads}"
                    );
                }
            }
        }
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
        size_exact: true,
        live: true,
    });
    view.delta = Arc::new(delta);
    let encoded = view.delta.encode_query_overlay();
    let decoded = Delta::decode_query_overlay(&encoded, view.base.archived().len()).unwrap();
    for query in [
        Query::Prefix("match".into()),
        Query::Substring("DELTA".into()),
        Query::wildcard("*.txt"),
        crate::terms::parse_query("match type:file ext:txt", 0).unwrap(),
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
    assert!(spans.select_ns > 0);
    assert!(spans.finalize_ns > 0);
    assert_eq!(spans.candidates, 100);
    assert_eq!(spans.blocks_scanned, spans.blocks_total);
}
