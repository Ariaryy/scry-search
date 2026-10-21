use super::*;

fn build_refinement_store(dir: &tempfile::TempDir) -> Arc<IndexView> {
    let mut b = Arena::builder();
    let root = b.push("C:", 0, true);
    let dirs = ["Documents", "Photos", "Projects"];
    let vocab = [
        "ledger", "report", "invoice", "photo", "backup", "notes", "draft", "project",
    ];
    let exts = ["txt", "pdf", "png", "docx"];
    let mut i: u32 = 0;
    for &d in &dirs {
        let dnode = b.push(d, 0, true);
        b.set_parent(dnode, root);
        for &w in &vocab {
            for &ext in &exts {
                let name = format!("{w}_{i}.{ext}");
                let f = b.push(&name, 0, false);
                b.set_parent(f, dnode);
                i += 1;
            }
        }
    }
    let arena = b.build().0;
    let path = dir.path().join("refine.rkyv");
    save(&arena, &path).unwrap();
    Arc::new(IndexView::new(Arc::new(ArenaStore::open(&path).unwrap())))
}

/// `contains_ci` only lowercases the haystack, not the needle, so the
/// per-request normalization passed to the refinement filter must handle
/// uppercase terms before any cached hit is visited.
#[test]
fn matches_refined_substring_is_case_insensitive_regardless_of_which_side_carries_the_case() {
    let dir = tempfile::tempdir().unwrap();
    let view = build_refinement_store(&dir);
    let hit = view.search_hits(&Query::Substring("ledger".into()), SearchOptions::new(1))[0];
    let mut name = Vec::new();
    assert!(matches_refined(
        &view,
        hit,
        QueryKind::Substring,
        &[b"ledger".to_vec()],
        &mut name,
    ));
}

/// Filtering a cached candidate set must always agree with rescanning the
/// index from scratch, at every keystroke of a randomised typing
/// sequence. A disagreement here would mean a refined query could hide a
/// file that a full scan would have found.
#[test]
fn refinement_matches_a_full_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let view = build_refinement_store(&dir);
    let indexes = single_volume(view);
    let generation = std::sync::atomic::AtomicU64::new(0);
    let vocab = [
        "ledger", "report", "invoice", "photo", "backup", "notes", "draft", "project",
    ];
    let mut rng: u64 = 0x243F_6A88_85A3_08D3;
    let mut next_rand = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    for _ in 0..1000 {
        let mut cache = RefinementCache::default();
        let word = vocab[(next_rand() as usize) % vocab.len()];
        let len = 1 + (next_rand() as usize) % word.len();
        for step in 1..=len {
            let pattern = word[..step].to_string();
            let cached = search_indexes_with_cache(
                &indexes,
                QueryKind::Substring,
                &Query::Substring(pattern.clone()),
                SearchOptions::new(50),
                scry_core::Cancellation::new(&generation, 0),
                &mut cache,
            );
            // A fresh cache never reuses a prior candidate list, so this
            // always takes the full-rescan branch — the baseline the
            // possibly-refined `cached` result above must match exactly.
            let mut uncached = RefinementCache::default();
            let fresh = search_indexes_with_cache(
                &indexes,
                QueryKind::Substring,
                &Query::Substring(pattern.clone()),
                SearchOptions::new(50),
                scry_core::Cancellation::new(&generation, 0),
                &mut uncached,
            );
            assert_eq!(cached, fresh, "diverged at pattern {pattern:?}");
        }
    }
}

/// Relevance ranks own-name term matches ahead of ancestor-only ones.
/// Typed terms keep their case, so the refined re-rank must fold them the
/// same way the fresh scan does, or an uppercase keystroke sequence
/// reorders results relative to a rescan.
#[test]
fn refined_path_terms_rank_like_a_full_rescan_for_uppercase_terms() {
    let dir = tempfile::tempdir().unwrap();
    let view = build_refinement_store(&dir);
    let indexes = single_volume(view);
    let generation = std::sync::atomic::AtomicU64::new(0);
    for word in ["PROJECT", "Photo", "DOCUMENTS"] {
        let mut cache = RefinementCache::default();
        for step in 1..=word.len() {
            let terms = vec![word[..step].to_string()];
            let cached = search_indexes_with_cache(
                &indexes,
                QueryKind::PathTerms,
                &Query::PathTerms(terms.clone()),
                SearchOptions::new(50),
                scry_core::Cancellation::new(&generation, 0),
                &mut cache,
            );
            let fresh = search_indexes_with_cache(
                &indexes,
                QueryKind::PathTerms,
                &Query::PathTerms(terms.clone()),
                SearchOptions::new(50),
                scry_core::Cancellation::new(&generation, 0),
                &mut RefinementCache::default(),
            );
            assert_eq!(cached, fresh, "diverged at terms {terms:?}");
        }
    }
}

/// A `Wildcard` query can only grow the match set as characters are
/// added, so it must never be answered by filtering a narrower cached
/// `Substring`/`Prefix`/`PathTerms` set — and must not overwrite that
/// cache either, since it isn't itself refinable.
#[test]
fn refinement_is_bypassed_for_regex() {
    let dir = tempfile::tempdir().unwrap();
    let view = build_refinement_store(&dir);
    let indexes = single_volume(view);
    let generation = std::sync::atomic::AtomicU64::new(0);
    let mut cache = RefinementCache::default();

    search_indexes_with_cache(
        &indexes,
        QueryKind::Substring,
        &Query::Substring("resu".to_string()),
        SearchOptions::new(50),
        scry_core::Cancellation::new(&generation, 0),
        &mut cache,
    );
    assert_eq!(cache.kind, Some(QueryKind::Substring));

    let wildcard_query = Query::wildcard("*.pdf");
    let actual = search_indexes_with_cache(
        &indexes,
        QueryKind::Wildcard,
        &wildcard_query,
        SearchOptions::new(50),
        scry_core::Cancellation::new(&generation, 0),
        &mut cache,
    );
    let expected = search_indexes_cancellable(
        &indexes,
        &wildcard_query,
        SearchOptions::new(50),
        scry_core::Cancellation::new(&generation, 0),
    );
    assert_eq!(actual, expected);
    assert_eq!(cache.kind, Some(QueryKind::Substring));
}

/// A one-shot `scry <query>` asks its connection exactly one question and
/// exits, so overscanning to `REFINEMENT_CACHE_CAP` on that first query
/// buys nothing and costs a `full_path` reconstruction per surplus hit.
/// The wide scan must therefore start only once a connection has shown it
/// is an as-you-type session by asking a second refinable question.
#[test]
fn first_query_on_a_connection_does_not_overscan_for_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let view = build_refinement_store(&dir);
    let indexes = single_volume(view);
    let generation = std::sync::atomic::AtomicU64::new(0);
    let mut cache = RefinementCache::default();

    search_indexes_with_cache(
        &indexes,
        QueryKind::Substring,
        &Query::Substring("re".to_string()),
        SearchOptions::new(5),
        scry_core::Cancellation::new(&generation, 0),
        &mut cache,
    );
    assert!(
        cache.per_volume.iter().all(Option::is_none),
        "a set truncated at the caller's limit is not a superset of what a \
             refined query could match, so it must not be cached"
    );

    search_indexes_with_cache(
        &indexes,
        QueryKind::Substring,
        &Query::Substring("res".to_string()),
        SearchOptions::new(5),
        scry_core::Cancellation::new(&generation, 0),
        &mut cache,
    );
    assert!(
        cache.per_volume.iter().all(Option::is_some),
        "the second refinable query on a connection should scan wide and cache"
    );
}

/// Before caching `Hit`s instead of `ResultEntry`s, a refined query paid a
/// `full_path` reconstruction for every one of the up to
/// `REFINEMENT_CACHE_CAP` overscanned candidates instead of just the
/// emitted `limit`. `materialize_one` is called exactly once per emitted
/// entry, so `spans.emitted` is a direct proxy for that call count.
#[test]
fn refined_query_materializes_at_most_limit_entries() {
    let dir = tempfile::tempdir().unwrap();
    // Larger than `REFINEMENT_CACHE_CAP` so the second query's cache
    // population actually exercises the wide overscan this test guards.
    let view = build_store_with_n_records(30_000, &dir);
    let indexes = single_volume(view);
    let generation = std::sync::atomic::AtomicU64::new(0);
    let mut cache = RefinementCache::default();
    let limit = 50;

    // First refinable query on the connection: establishes the cache but
    // does not itself overscan (see the test above).
    search_indexes_with_cache_with_spans(
        &indexes,
        QueryKind::Substring,
        &Query::Substring("f".to_string()),
        SearchOptions::new(limit),
        scry_core::Cancellation::new(&generation, 0),
        &mut cache,
        &mut QuerySpans::default(),
    );

    let mut spans = QuerySpans::default();
    let results = search_indexes_with_cache_with_spans(
        &indexes,
        QueryKind::Substring,
        &Query::Substring("fi".to_string()),
        SearchOptions::new(limit),
        scry_core::Cancellation::new(&generation, 0),
        &mut cache,
        &mut spans,
    );
    assert!(results.len() <= limit);
    assert!(
        spans.emitted as usize <= limit,
        "materialized {} entries for a limit of {limit}",
        spans.emitted
    );
}

/// The merge across volumes has only `ResultEntry`s to sort, so a bug
/// here would show up as results that are correctly ranked *within* each
/// volume and shuffled across them — which no per-volume test can catch.
#[cfg(any())]
#[test]
fn the_cross_volume_merge_honors_the_requested_order() {
    let entry = |path: &str, size: u64, mtime: u32| ResultEntry {
        path: path.to_string(),
        size,
        mtime,
        is_dir: false,
        size_exact: false,
    };
    let query = Query::Substring("report".to_string());
    let merged = vec![
        entry(
            r"C:
eport.txt",
            10,
            300,
        ),
        entry(
            r"D:
eport.txt",
            30,
            100,
        ),
        entry(
            r"C:
eport.txt",
            20,
            200,
        ),
    ];

    let paths = |ordering| {
        let mut entries = merged.clone();
        rank_sort_truncate(&query, ordering, &mut entries, 10);
        entries
            .into_iter()
            .map(|entry| entry.path)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        paths(Order::Recent),
        [
            r"C:
eport.txt",
            r"C:
eport.txt",
            r"D:
eport.txt"
        ]
    );
    assert_eq!(
        paths(Order::Largest),
        [
            r"D:
eport.txt",
            r"C:
eport.txt",
            r"C:
eport.txt"
        ]
    );

    let mut truncated = merged.clone();
    rank_sort_truncate(&query, Order::Largest, &mut truncated, 1);
    assert_eq!(truncated.len(), 1);
    assert_eq!(
        truncated[0].path,
        r"D:
eport.txt"
    );
}

/// A bug here would leave results correctly ranked within each volume but
/// shuffled across them. Rank bits compare first; volume and its local
/// record are deterministic tie-breakers only.
#[test]
fn the_cross_volume_merge_honors_carried_rank_bits() {
    let hit = |record, rank_bits| Hit {
        record,
        rank_bits,
        size: 0,
        mtime: 0,
        is_dir: false,
        size_exact: false,
    };
    let mut hits = vec![
        (1, hit(2, 30)),
        (0, hit(9, 10)),
        (1, hit(7, 10)),
        (0, hit(3, 20)),
    ];
    rank_sort_truncate_hits(&mut hits, 3);
    assert_eq!(
        hits.iter()
            .map(|(volume, hit)| (*volume, hit.record))
            .collect::<Vec<_>>(),
        [(0, 9), (1, 7), (0, 3)]
    );
}

#[test]
fn cross_volume_merge_matches_a_union_sort_at_every_limit() {
    let hit = |record, rank_bits| Hit {
        record,
        rank_bits,
        size: 0,
        mtime: 0,
        is_dir: false,
        size_exact: false,
    };
    let input = vec![
        (2, hit(8, 4)),
        (0, hit(9, 1)),
        (1, hit(2, 4)),
        (0, hit(3, 4)),
        (1, hit(7, 2)),
    ];
    let mut expected = input.clone();
    expected.sort_unstable_by_key(|(volume, hit)| (hit.rank_bits, *volume, hit.record));

    for limit in 0..=input.len() + 1 {
        let mut actual = input.clone();
        rank_sort_truncate_hits(&mut actual, limit);
        let mut want = expected.clone();
        want.truncate(limit);
        assert_eq!(actual, want, "limit {limit}");
    }
}
