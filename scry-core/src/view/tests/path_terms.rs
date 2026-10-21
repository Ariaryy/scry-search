use super::*;

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
        size_exact: true,
        live: true,
    });
    delta.added.push(DeltaRecord {
        name: "y".into(),
        parent: ParentRef::Delta(0),
        mtime_secs: 0,
        is_dir: false,
        size_bytes: 0,
        size_exact: true,
        live: true,
    });
    view.delta = Arc::new(delta);
    assert!(view.delta_path(1).ends_with("X\\y"));
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
        size_exact: true,
        live: true,
    });
    let delta_dir_index = (delta.added.len() - 1) as u32;
    delta.added.push(DeltaRecord {
        name: "deltaleafwkyp.txt".into(),
        parent: ParentRef::Delta(delta_dir_index),
        mtime_secs: 0,
        is_dir: false,
        size_bytes: 0,
        size_exact: true,
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
        size_exact: true,
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
        vec!["cycleb".to_string(), "cyclechild".to_string()],
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
fn path_term_hit_order_matches_bruteforce_for_every_order() {
    let (_dir, view) = adversarial_path_term_view();
    let arena = view.base.archived();
    let base_len = arena.len() as u32;
    let total = base_len + view.delta.added.len() as u32;

    for terms in [
        vec!["alpha".to_owned()],
        vec!["cycleb".to_owned(), "cyclechild".to_owned()],
        vec!["deltadirqzxv".to_owned(), "deltaleafwkyp".to_owned()],
    ] {
        let terms_lower: Vec<_> = terms
            .iter()
            .map(|term| term.as_bytes().to_ascii_lowercase())
            .collect();
        for order in [Order::Relevance, Order::Recent, Order::Largest] {
            let mut expected = Vec::new();
            let mut name = Vec::new();
            for record in 0..total {
                let live = if record < base_len {
                    !view.delta.tombstones.get(record)
                } else {
                    view.delta.added[(record - base_len) as usize].live
                };
                if !live {
                    continue;
                }
                let path_lower = view.path_of(record).to_ascii_lowercase();
                if !terms.iter().all(|term| path_lower.contains(term)) {
                    continue;
                }
                view.name_into(record, &mut name);
                let own = terms_lower
                    .iter()
                    .filter(|term| crate::ascii::contains_ci(&name, term))
                    .count();
                let quality = (terms.len() - own) as u8;
                expected.push(sort_key(
                    order,
                    arena,
                    &view.delta,
                    record,
                    quality,
                    name.len() as u32,
                ));
            }
            expected.sort_unstable();
            let expected: Vec<_> = expected.into_iter().map(rank::key_record).collect();
            for limit in [0usize, 1, 50, usize::MAX] {
                let expected = &expected[..expected.len().min(limit)];
                for threads in [1usize, 2, 4, 8] {
                    let actual: Vec<_> = view
                        .search_hits_cancellable(
                            &Query::PathTerms(terms.clone()),
                            SearchOptions::ordered(limit, order),
                            None,
                            threads,
                        )
                        .into_iter()
                        .map(|hit| hit.record)
                        .collect();
                    assert_eq!(
                        actual, expected,
                        "terms={terms:?}, order={order:?}, limit={limit}, threads={threads}"
                    );
                }
            }
        }
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
fn large_path_queries_release_scratch_and_small_queries_remain_reusable() {
    let (_dir, view) = base_view(150_000);
    let small = Query::PathTerms(vec!["match_000".to_owned()]);
    let expected_small = view.search(&small, 50);
    PATH_SEARCH_SCRATCH.with(|scratch| {
        let bytes = scratch.borrow().allocated_bytes();
        assert!(bytes > 0 && bytes <= PATH_SCRATCH_RETAINED_BYTES);
    });
    let broad = Query::PathTerms(vec!["match".to_owned()]);
    let first = view.search(&broad, 200);
    assert_eq!(first.len(), 200);
    PATH_SEARCH_SCRATCH.with(|scratch| assert_eq!(scratch.borrow().allocated_bytes(), 0));
    assert_eq!(view.search(&broad, 200), first);
    PATH_SEARCH_SCRATCH.with(|scratch| assert_eq!(scratch.borrow().allocated_bytes(), 0));
    assert_eq!(view.search(&small, 50), expected_small);
    PATH_SEARCH_SCRATCH.with(|scratch| {
        assert!(scratch.borrow().allocated_bytes() <= PATH_SCRATCH_RETAINED_BYTES);
    });
}

#[test]
fn cancelled_path_query_releases_oversized_scratch() {
    let (_dir, view) = base_view(20);
    PATH_SEARCH_SCRATCH.with(|scratch| {
        scratch.borrow_mut().own_matches = vec![0; PATH_SCRATCH_RETAINED_BYTES + 1];
    });
    let generation = AtomicU64::new(1);
    let hits = search_path_terms_filtered_cancellable(
        view.base.archived(),
        &view.delta,
        &["match".to_owned()],
        &QueryFilter::default(),
        SearchOptions::new(10),
        Some(Cancellation::new(&generation, 0)),
        1,
    );
    assert!(hits.is_empty());
    PATH_SEARCH_SCRATCH.with(|scratch| assert_eq!(scratch.borrow().allocated_bytes(), 0));
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
        &QueryFilter::default(),
        SearchOptions::new(usize::MAX),
        &mut PathSearchScratch::default(),
        true,
        None,
        1,
    );
    let full = search_path_terms_with_scratch(
        view.base.archived(),
        &view.delta,
        &terms,
        &QueryFilter::default(),
        SearchOptions::new(usize::MAX),
        &mut PathSearchScratch::default(),
        false,
        None,
        1,
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
        &QueryFilter::default(),
        SearchOptions::new(10),
        &mut scratch,
        true,
        None,
        1,
    );
    assert_eq!(absent.len(), 10);

    let results = search_path_terms_with_scratch(
        small.base.archived(),
        &small.delta,
        &["definitely_absent_qzxv".to_owned()],
        &QueryFilter::default(),
        SearchOptions::new(10),
        &mut scratch,
        true,
        None,
        1,
    );
    assert!(results.is_empty());
    // A single-term query leaves exactly one term set behind, sized to
    // the smaller index rather than any leftover capacity from the
    // larger one.
    assert_eq!(scratch.term_sets.len(), 1);
    assert!(scratch.term_sets[0].is_empty());
}

mod enumeration;
mod planner;
