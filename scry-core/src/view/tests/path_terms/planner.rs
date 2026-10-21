use super::*;

#[test]
fn globally_absent_path_term_skips_later_term_scans() {
    let (_dir, view) = base_view(2_000);
    TERM_INTERVAL_BUILDS.with(|count| count.set(0));

    let results = view.search_hits(
        &Query::PathTerms(vec!["globally_absent_qzxv".to_owned(), "match".to_owned()]),
        SearchOptions::new(10),
    );

    assert!(results.is_empty());
    TERM_INTERVAL_BUILDS.with(|count| assert_eq!(count.get(), 1));
}

#[test]
fn planned_path_terms_match_exhaustive_with_tombstones_and_cycles() {
    let (_dir, mut view) = adversarial_path_term_view();
    let mut delta = (*view.delta).clone();
    delta.added.clear();
    // A deleted directory's own term must not pass to its descendants.
    delta
        .tombstones
        .set(view.base.archived().prefix_range("bravo").start);
    view.delta = Arc::new(delta);
    let cases = [
        vec!["ALPHA", "deepfile"],
        vec!["bravo", "deepfile"],
        vec!["cyclea", "cyclechild"],
        vec!["cycleb", "cyclechild"],
        vec!["selfparented", "self"],
        vec!["c:", "leafonlywkyp"],
        vec!["plain", "leafonlywkyp"],
        vec!["alpha", "alpha"],
        vec![""; 16],
    ];
    for case in cases {
        let terms: Vec<_> = case.into_iter().map(str::to_owned).collect();
        for filter in [
            QueryFilter::default(),
            QueryFilter {
                kind: Some(EntryKind::File),
                ..QueryFilter::default()
            },
            QueryFilter {
                extensions: vec!["TXT".into()],
                ..QueryFilter::default()
            },
            QueryFilter {
                min_mtime: Some(1),
                ..QueryFilter::default()
            },
        ] {
            for order in [Order::Relevance, Order::Recent, Order::Largest] {
                for limit in [1, 4, 20_000] {
                    let mut scratch = PathSearchScratch::default();
                    let expected = search_path_terms_with_scratch(
                        view.base.archived(),
                        &view.delta,
                        &terms,
                        &filter,
                        SearchOptions::ordered(limit, order),
                        &mut scratch,
                        false,
                        None,
                        1,
                    );
                    for threads in [1, 4] {
                        let actual = search_path_terms_with_scratch(
                            view.base.archived(),
                            &view.delta,
                            &terms,
                            &filter,
                            SearchOptions::ordered(limit, order),
                            &mut scratch,
                            true,
                            None,
                            threads,
                        );
                        assert_eq!(actual, expected, "{terms:?}, {filter:?}, {order:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn sparse_path_planner_skips_scans_but_keeps_delta_ancestor_sets() {
    let (_dir, view) = adversarial_path_term_view();
    let mut delta = (*view.delta).clone();
    delta.added.clear();
    let terms = vec!["alpha".into(), "deepfile".into()];
    for (overlay, expected_builds) in [(&delta, 1), (&*view.delta, 2)] {
        TERM_INTERVAL_BUILDS.with(|count| count.set(0));
        let actual = search_path_terms_with_scratch(
            view.base.archived(),
            overlay,
            &terms,
            &QueryFilter::default(),
            SearchOptions::new(200),
            &mut PathSearchScratch::default(),
            true,
            None,
            1,
        );
        assert_eq!(actual.len(), 1);
        TERM_INTERVAL_BUILDS.with(|count| assert_eq!(count.get(), expected_builds));
    }
}

#[test]
fn path_seed_budgets_discard_partial_verification() {
    let (_dir, view) = nested_view();
    let arena = view.base.archived();
    let mut seed = crate::intervals::IntervalSet::default();
    seed.push_span(0, arena.len() as u32);
    seed.coalesce();
    let automaton = aho_corasick::AhoCorasick::new(["missing"]).unwrap();
    let mut output = crate::intervals::IntervalSet::default();
    let mut name = Vec::new();
    assert_eq!(
        verify_path_seed(
            arena,
            &view.delta,
            &seed,
            &automaton,
            1,
            &mut output,
            &mut name,
            || false,
            0
        ),
        Ok(false)
    );
    // Change the generation after verification has entered the walk.
    let generation = AtomicU64::new(0);
    let cancel = Cancellation::new(&generation, 0);
    let mut polls = 0;
    assert_eq!(
        verify_path_seed(
            arena,
            &view.delta,
            &seed,
            &automaton,
            1,
            &mut output,
            &mut name,
            || {
                polls += 1;
                if polls == 3 {
                    generation.store(1, AtomicOrdering::Relaxed);
                }
                cancel.is_cancelled()
            },
            PATH_VERIFY_STEPS
        ),
        Err(())
    );
    assert_eq!(polls, 3);
    let generation = AtomicU64::new(1);
    assert_eq!(
        verify_path_seed(
            arena,
            &view.delta,
            &seed,
            &automaton,
            1,
            &mut output,
            &mut name,
            || Cancellation::new(&generation, 0).is_cancelled(),
            PATH_VERIFY_STEPS
        ),
        Err(())
    );
    seed.clear();
    seed.push_span(0, PATH_VERIFY_CANDIDATES as u32 + 1);
    seed.coalesce();
    assert_eq!(
        verify_path_seed(
            arena,
            &view.delta,
            &seed,
            &automaton,
            1,
            &mut output,
            &mut name,
            || false,
            PATH_VERIFY_STEPS
        ),
        Ok(false)
    );
}

#[test]
fn deep_path_seed_falls_back_without_losing_ancestor_matches() {
    let mut builder = crate::ArenaBuilder::default();
    let mut parent = builder.push("ancestorqzxv", 0, true);
    for index in 0..PATH_VERIFY_DEPTH + 20 {
        let next = builder.push(&format!("level_{index:04}"), 0, true);
        builder.set_parent(next, parent);
        parent = next;
    }
    let leaf = builder.push("needlewkyp", 0, false);
    builder.set_parent(leaf, parent);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deep-planner.rkyv");
    crate::store::save(&builder.build().0, &path).unwrap();
    let view = IndexView::new(Arc::new(ArenaStore::open(&path).unwrap()));
    TERM_INTERVAL_BUILDS.with(|count| count.set(0));
    let actual = view.search_hits(
        &Query::PathTerms(vec!["needlewkyp".into(), "ancestorqzxv".into()]),
        SearchOptions::new(200),
    );
    assert_eq!(actual.len(), 1);
    TERM_INTERVAL_BUILDS.with(|count| assert_eq!(count.get(), 2));
}

#[test]
fn path_planner_matches_fold_across_sparse_and_broad_seed_sets() {
    let mut builder = crate::ArenaBuilder::default();
    let root = builder.push("common", 0, true);
    let mut parents = Vec::new();
    for index in 0..128 {
        let name = format!(
            "{}_dir_{index:04}",
            if index % 2 == 0 { "even" } else { "odd" }
        );
        let parent = builder.push(&name, 0, true);
        builder.set_parent(parent, root);
        parents.push(parent);
    }
    for index in 0..8_192 {
        let name = if index % 97 == 0 {
            "needle.bin"
        } else {
            "plain.txt"
        };
        let record = builder.push_bytes_with_metadata(
            name.as_bytes(),
            index,
            false,
            None,
            u64::from(index) * 1024,
        );
        builder.set_parent(record, parents[index as usize * 37 % parents.len()]);
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("planner-strata.rkyv");
    crate::store::save(&builder.build().0, &path).unwrap();
    let view = IndexView::new(Arc::new(ArenaStore::open(&path).unwrap()));
    let arena = view.base.archived();
    let mut delta = (*view.delta).clone();
    for record in (0..arena.len() as u32).step_by(7) {
        delta.tombstones.set(record);
    }
    let mut scratch = PathSearchScratch::default();
    for terms in [
        vec!["odd", "needle"],
        vec!["EVEN", "needle"],
        vec!["common", "plain"],
        vec!["common", "needle"],
        vec!["needle", "bin"],
        vec!["absent", "needle"],
    ] {
        let terms: Vec<_> = terms.into_iter().map(str::to_owned).collect();
        for order in [Order::Relevance, Order::Recent, Order::Largest] {
            for filter in [
                QueryFilter::default(),
                QueryFilter {
                    min_size: Some(100_000),
                    extensions: vec!["bin".into()],
                    ..QueryFilter::default()
                },
            ] {
                for limit in [1, 200, 20_000] {
                    let options = SearchOptions::ordered(limit, order);
                    let expected = search_path_terms_with_scratch(
                        arena,
                        &delta,
                        &terms,
                        &filter,
                        options,
                        &mut scratch,
                        false,
                        None,
                        1,
                    );
                    for threads in [1, 4] {
                        let actual = search_path_terms_with_scratch(
                            arena,
                            &delta,
                            &terms,
                            &filter,
                            options,
                            &mut scratch,
                            true,
                            None,
                            threads,
                        );
                        assert_eq!(
                            actual, expected,
                            "{terms:?}, {filter:?}, {order:?}, {limit}"
                        );
                    }
                }
            }
        }
    }
}
