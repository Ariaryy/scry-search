use super::*;

#[test]
fn dense_path_term_enumeration_matches_bruteforce() {
    check_path_term_enumeration(0);
}

#[test]
fn sparse_path_term_batches_match_bruteforce() {
    check_path_term_enumeration(60_000);
}

#[test]
fn dense_record_bitmap_preserves_dfs_membership_and_partial_bytes() {
    let (_dir, view) = base_view(258);
    let arena = view.base.archived();
    assert!(!arena.len().is_multiple_of(8));
    for stride in [1, 3, 31, arena.len() + 1] {
        let mut runs = crate::intervals::IntervalSet::default();
        for position in (0..arena.len() as u32).step_by(stride) {
            runs.push_point(position);
        }
        runs.coalesce();
        let bits = path_record_bitmap(arena, &runs, None).unwrap();
        assert_eq!(bits.len(), arena.len().div_ceil(8));
        for record in 0..arena.len() as u32 {
            assert_eq!(
                bits[record as usize / 8] & (1 << (record % 8)) != 0,
                runs.contains(arena.dfs_position(record)),
                "stride={stride}, record={record}"
            );
        }
        for unused in arena.len()..bits.len() * 8 {
            assert_eq!(bits[unused / 8] & (1 << (unused % 8)), 0);
        }
    }
    let empty = crate::intervals::IntervalSet::default();
    let bits = path_record_bitmap(arena, &empty, None).unwrap();
    assert!(bits.iter().all(|&byte| byte == 0));
    let generation = AtomicU64::new(1);
    assert!(path_record_bitmap(arena, &empty, Some(Cancellation::new(&generation, 0))).is_none());
}

#[test]
fn path_candidate_visits_cover_batch_density_and_bucket_boundaries() {
    let (_dir, view) = base_view(150_000);
    let arena = view.base.archived();
    let boundary = arena.len() / 32;
    let mut name = Vec::new();
    for count in [
        0,
        1,
        31,
        32,
        33,
        1024,
        1025,
        4096,
        4097,
        boundary,
        boundary + 1,
    ] {
        let mut runs = crate::intervals::IntervalSet::default();
        let mut expected = Vec::new();
        for position in (0..arena.len() as u32).step_by(31).take(count) {
            runs.push_point(position);
            expected.push(arena.dfs_record(position));
        }
        runs.coalesce();
        let mut actual = Vec::new();
        assert!(visit_path_candidate_names(
            arena,
            &runs,
            &mut name,
            None,
            |record, name, _| {
                let mut expected_name = Vec::new();
                arena.name_into(record, &mut expected_name);
                assert_eq!(name, expected_name);
                actual.push(record);
            }
        ));
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected, "count={count}");
    }

    let mut runs = crate::intervals::IntervalSet::default();
    runs.push_span(1, PATH_RECORD_BATCH as u32 + 2);
    runs.coalesce();
    assert!(use_path_record_batches(arena.len(), runs.len_positions()));
    let generation = AtomicU64::new(0);
    let cancel = Cancellation::new(&generation, 0);
    let mut visited = 0;
    assert!(!visit_path_candidate_names(
        arena,
        &runs,
        &mut name,
        Some(cancel),
        |_, _, _| {
            visited += 1;
            generation.store(1, AtomicOrdering::Relaxed);
        }
    ));
    assert!((1..=crate::record::BUCKET_SIZE).contains(&visited));
}

#[test]
fn dense_path_cancellation_stops_at_the_next_bucket() {
    let (_dir, view) = base_view(4100);
    let arena = view.base.archived();
    let mut runs = crate::intervals::IntervalSet::default();
    runs.push_span(0, arena.len() as u32);
    runs.coalesce();
    assert!(!use_path_record_batches(arena.len(), runs.len_positions()));
    for signal_at in [1, 31, 32, 33, arena.len()] {
        let generation = AtomicU64::new(0);
        let mut visited = 0;
        let completed = visit_path_candidate_names(
            arena,
            &runs,
            &mut Vec::new(),
            Some(Cancellation::new(&generation, 0)),
            |_, _, _| {
                visited += 1;
                if visited == signal_at {
                    generation.store(1, AtomicOrdering::Relaxed);
                }
            },
        );
        // The caller discards its partial heap when completion is false.
        assert!(!completed, "signal_at={signal_at}");
        assert!(visited >= signal_at);
        assert!(visited - signal_at < crate::record::BUCKET_SIZE);
    }
}

#[test]
fn dense_path_cancellation_polls_inside_empty_bitmap_regions() {
    let (_dir, view) = base_view(4100);
    let arena = view.base.archived();
    for occupied_edges in [false, true] {
        let mut bits = vec![0u8; arena.len().div_ceil(8)];
        if occupied_edges {
            bits[0] = 1;
            let last = arena.len() - 1;
            bits[last / 8] |= 1 << (last % 8);
        }
        // Trigger the generation change at different points inside a long
        // empty region. No sleeps or scheduling races are involved.
        for signal_poll in [2, 33, 64, 100] {
            let generation = AtomicU64::new(0);
            let cancel = Cancellation::new(&generation, 0);
            let mut polls = 0;
            let mut visited = 0;
            assert!(!visit_path_bitmap_names(
                arena,
                &bits,
                &mut Vec::new(),
                || {
                    polls += 1;
                    if polls == signal_poll {
                        generation.store(1, AtomicOrdering::Relaxed);
                    }
                    cancel.is_cancelled()
                },
                |_, _, _| visited += 1,
            ));
            assert_eq!(polls, signal_poll);
            assert_eq!(visited, usize::from(occupied_edges));
        }
    }
}

/// The same matches take the bitmap route in the compact corpus and
/// bounded batches with unrelated padding. Check both against full paths,
/// across orders, metadata filters, limits, threads and live overlays.
fn check_path_term_enumeration(padding: usize) {
    let mut builder = crate::ArenaBuilder::default();
    let root = builder.push("C:", 0, true);
    for index in 0..padding {
        let record = builder.push(&format!("padding_{index:06}.bin"), 0, false);
        builder.set_parent(record, root);
    }
    let mut file = 0u64;
    for group in 0..24u32 {
        let group_name = if group % 3 == 0 {
            format!("zonegroup_{group:02}")
        } else {
            format!("plaingroup_{group:02}")
        };
        let group_dir =
            builder.push_bytes_with_metadata(group_name.as_bytes(), group, true, None, 0);
        builder.set_parent(group_dir, root);
        for sub in 0..4u32 {
            let sub_name = format!("bucket_{group:02}_{sub}");
            let sub_dir = builder.push_bytes_with_metadata(
                sub_name.as_bytes(),
                group * 10 + sub,
                true,
                None,
                0,
            );
            builder.set_parent(sub_dir, group_dir);
            for leaf in 0..40u32 {
                file += 1;
                let extension = if leaf % 2 == 0 { "txt" } else { "bin" };
                let leaf_name = if leaf % 5 == 0 {
                    format!("zone_leaf_{leaf:02}.{extension}")
                } else {
                    format!("leaf_{leaf:02}.{extension}")
                };
                let record = builder.push_bytes_with_metadata(
                    leaf_name.as_bytes(),
                    (file * 7_919 % 100_000) as u32,
                    false,
                    None,
                    file * 1_543 % 9_000_000,
                );
                builder.set_parent(record, sub_dir);
            }
        }
    }
    let (arena, _) = builder.build();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dense-path-terms.rkyv");
    crate::store::save(&arena, &path).unwrap();
    let base = Arc::new(ArenaStore::open(&path).unwrap());
    let mut view = IndexView::new(base);
    let base_len = view.base.archived().len() as u32;
    let mut delta = Delta::new(base_len as usize);
    // Deleted files only: a tombstoned directory intentionally stops
    // passing its name to descendants, which a full-path oracle can't see.
    for record in (0..base_len).step_by(7) {
        if !view.base.archived().is_dir(record) {
            delta.tombstones.set(record);
        }
    }
    let parent = view.base.archived().prefix_range("zonegroup_").start;
    for (name, live) in [
        ("zone_bucket_delta.txt", true),
        ("zone_bucket_deleted.txt", false),
    ] {
        delta.added.push(DeltaRecord {
            name: name.to_owned(),
            parent: ParentRef::Base(parent),
            mtime_secs: 55_000,
            is_dir: false,
            size_bytes: 3_000_000,
            size_exact: true,
            live,
        });
    }
    view.delta = Arc::new(delta);
    let arena = view.base.archived();

    let filters = [
        QueryFilter::default(),
        QueryFilter {
            kind: Some(EntryKind::File),
            ..QueryFilter::default()
        },
        QueryFilter {
            extensions: vec!["TXT".to_owned()],
            ..QueryFilter::default()
        },
        QueryFilter {
            min_size: Some(2_000_000),
            max_mtime: Some(60_000),
            ..QueryFilter::default()
        },
    ];
    for terms in [
        vec!["zone".to_owned()],
        vec!["ZONE".to_owned(), "bucket".to_owned()],
    ] {
        let terms_lower: Vec<_> = terms
            .iter()
            .map(|term| term.as_bytes().to_ascii_lowercase())
            .collect();
        for filter in &filters {
            for order in [Order::Relevance, Order::Recent, Order::Largest] {
                let mut expected = Vec::new();
                let mut name = Vec::new();
                for record in 0..base_len + view.delta.added.len() as u32 {
                    if view.delta.tombstones.get(record)
                        || (record >= base_len
                            && !view.delta.added[(record - base_len) as usize].live)
                    {
                        continue;
                    }
                    let path_lower = view.path_of(record).to_ascii_lowercase();
                    if !terms_lower
                        .iter()
                        .all(|term| path_lower.contains(std::str::from_utf8(term).unwrap()))
                    {
                        continue;
                    }
                    view.name_into(record, &mut name);
                    if !record_matches_filter(arena, &view.delta, record, &name, filter) {
                        continue;
                    }
                    let own = terms_lower
                        .iter()
                        .filter(|term| crate::ascii::contains_ci(&name, term))
                        .count();
                    expected.push(sort_key(
                        order,
                        arena,
                        &view.delta,
                        record,
                        (terms.len() - own) as u8,
                        name.len() as u32,
                    ));
                }
                if filter.is_empty() {
                    assert!(expected.len() as u64 > SPARSE_PATH_HITS);
                    assert_eq!(
                        use_path_record_batches(arena.len(), expected.len() as u64),
                        padding != 0
                    );
                }
                expected.sort_unstable();
                let expected: Vec<_> = expected.into_iter().map(rank::key_record).collect();
                for limit in [1usize, 50, usize::MAX] {
                    let expected = &expected[..expected.len().min(limit)];
                    for threads in [1usize, 4] {
                        let actual: Vec<_> = search_path_terms_filtered_cancellable(
                            arena,
                            &view.delta,
                            &terms,
                            filter,
                            SearchOptions::ordered(limit, order),
                            None,
                            threads,
                        )
                        .into_iter()
                        .map(|hit| hit.record)
                        .collect();
                        assert_eq!(
                            actual, expected,
                            "terms={terms:?}, filter={filter:?}, order={order:?},                                  limit={limit}, threads={threads}"
                        );
                    }
                }
            }
        }
    }
}
