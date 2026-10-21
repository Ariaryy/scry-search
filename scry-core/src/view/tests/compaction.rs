use super::*;

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
        size_exact: true,
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

#[test]
fn compaction_removes_tombstones_and_empty_is_identity() {
    let (_dir, mut view) = base_view(20);
    let original = view.len();
    let (_empty_dir, empty) = open_compacted(&view);
    assert_eq!(empty.len(), original);

    let mut delta = Delta::new(view.base.archived().len());
    assert!(delta.tombstones.set(3));
    assert!(delta.tombstones.set(7));
    view.delta = Arc::new(delta);
    let (_compacted_dir, compacted) = open_compacted(&view);
    assert_eq!(compacted.len(), original - 2);
}

/// `compact`'s streaming merge/patch rewrite must produce exactly the
/// same records — same paths, same metadata, no duplicates or drops —
/// as a brute-force reference built directly from base plus delta, at
/// delta ratios from 0% up through and past the 5% compaction threshold,
/// with tombstones scattered through base so the merge has to skip
/// holes on both sides at once rather than only ever appending delta at
/// the end.
#[test]
fn streaming_compaction_matches_buffered() {
    const BASE: usize = 2_000;
    for percent in [0u32, 1, 5, 20] {
        let (_dir, mut view) = base_view(BASE);
        let root = view.base.archived().prefix_range("C:").start;
        let added = BASE * percent as usize / 100;

        let mut delta = Delta::new(view.base.archived().len());
        for i in (1..BASE as u32).step_by(3) {
            delta.tombstones.set(i);
        }
        for i in 0..added {
            delta.added.push(DeltaRecord {
                name: format!("delta_add_{i:04}.txt"),
                parent: ParentRef::Base(root),
                mtime_secs: 1_000 + i as u32,
                is_dir: false,
                size_bytes: (i as u64 + 1) * 4096,
                size_exact: true,
                live: true,
            });
        }
        view.delta = Arc::new(delta.clone());

        let arena = view.base.archived();
        let mut expected: Vec<(String, u32, u64, bool)> = (0..arena.len() as u32)
            .filter(|&i| !delta.tombstones.get(i))
            .map(|i| {
                (
                    arena.full_path(i, '\\'),
                    arena.mtime(i),
                    arena.size_bytes(i),
                    arena.is_dir(i),
                )
            })
            .collect();
        for (_, record) in view.delta.live_added() {
            let path = match record.parent {
                ParentRef::Base(p) => {
                    format!("{}\\{}", arena.full_path(p, '\\'), record.name)
                }
                ParentRef::Delta(_) | ParentRef::None => record.name.clone(),
            };
            expected.push((path, record.mtime_secs, record.size_bytes, record.is_dir));
        }
        expected.sort();

        let (_compacted_dir, compacted_view) = open_compacted(&view);
        let compacted = compacted_view.base.archived();
        let mut actual: Vec<(String, u32, u64, bool)> = (0..compacted.len() as u32)
            .map(|i| {
                (
                    compacted.full_path(i, '\\'),
                    compacted.mtime(i),
                    compacted.size_bytes(i),
                    compacted.is_dir(i),
                )
            })
            .collect();
        actual.sort();

        assert_eq!(actual, expected, "diverged at delta ratio {percent}%");
    }
}

#[test]
fn compaction_rederives_directory_size_exactness() {
    let (_dir, mut view) = base_view(20);
    let root = view.base.archived().prefix_range("C:").start;
    let mut delta = Delta::new(view.base.archived().len());
    delta.added.push(DeltaRecord {
        name: "unknown-added.bin".into(),
        parent: ParentRef::Base(root),
        mtime_secs: 0,
        is_dir: false,
        size_bytes: 64 * 1024,
        size_exact: false,
        live: true,
    });
    view.delta = Arc::new(delta);

    let (_compacted_dir, compacted) = open_compacted(&view);
    let arena = compacted.base.archived();
    let root = arena.prefix_range("C:").start;
    let unknown = arena.prefix_range("unknown-added.bin").start;
    assert!(!arena.size_exact(unknown));
    assert!(!arena.size_exact(root));
    assert!(arena.recursive_size_kib(root) >= 64);
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
        size_exact: true,
        live: true,
    });
    delta.added.push(DeltaRecord {
        name: "delta_grandchild".into(),
        parent: ParentRef::Delta(0),
        mtime_secs: 0,
        is_dir: false,
        size_bytes: 0,
        size_exact: true,
        live: true,
    });
    // Deliberately reverse FRN order relative to delta insertion order so
    // compaction must sort and merge both halves rather than accidentally
    // preserving the additions vector's order.
    delta.added_frns.insert(5_000, 0);
    delta.added_frns.insert(500, 1);
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

    let out_path = dir.path().join("compacted.rkyv");
    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    view.compact_to_snapshot(&scratch, &out_path, &|_| {}, &mut |_| {})
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
    let delta_child = compacted_frn_map.lookup(5_000).unwrap();
    let delta_grandchild = compacted_frn_map.lookup(500).unwrap();
    assert_eq!(
        compacted_archived.full_path(delta_child, '\\'),
        "C:\\delta_root_child"
    );
    assert_eq!(
        compacted_archived.full_path(delta_grandchild, '\\'),
        "C:\\delta_root_child\\delta_grandchild"
    );
}
