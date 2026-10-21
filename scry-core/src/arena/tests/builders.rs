use super::*;

/// `SpooledArenaBuilder` must produce the same columns as
/// `ArenaBuilder::build()` given the same records: push them into
/// `SpooledArenaBuilder` in final sorted order (mirroring what
/// `IndexView::compact`'s merge guarantees) with every parent left as a
/// placeholder, then patch every parent out of push order, then
/// `finish()` followed by the same file-backed DFS pass compaction runs.
/// `dfs::build_file_backed`'s own handling of malformed parent columns
/// (cycles, self-parents, dangling indices) is exercised directly in
/// `dfs.rs`'s tests against `dfs::build` — so this test's job is
/// narrower: prove `SpooledFrontCoder` matches the batch `front_code`
/// byte-for-byte, and that patching parents after the fact (rather than
/// knowing them at push time) still feeds the DFS pass the same column
/// the regular builder would have.
#[test]
fn streaming_builder_matches_regular_builder_output() {
    // (name, is_dir, mtime, size_bytes, raw parent by *input* index).
    // Input order is deliberately not name order, so building the
    // reference via `ArenaBuilder` (arbitrary push order) and the
    // streaming builder (records pushed in the name-sorted order below)
    // exercise genuinely different code paths.
    let records: &[(&str, bool, u32, u64, Option<usize>)] = &[
        ("C:", true, 0, 0, None),
        ("bravo", false, 200, 4096, Some(0)),
        ("alpha", true, 100, 0, Some(0)),
        ("charlie", false, 300, 8192, Some(2)), // child of "alpha"
        ("delta", false, 400, 0, Some(0)),
        ("echo", false, 500, 0, Some(2)), // sibling of "charlie"
    ];

    let mut regular = ArenaBuilder::with_capacity(records.len(), 0);
    let mut regular_indices = vec![0u32; records.len()];
    for (i, &(name, is_dir, mtime, size, _)) in records.iter().enumerate() {
        regular_indices[i] =
            regular.push_bytes_with_metadata(name.as_bytes(), mtime, is_dir, None, size);
    }
    for (i, &(_, _, _, _, parent)) in records.iter().enumerate() {
        if let Some(parent) = parent {
            regular.set_parent(regular_indices[i], regular_indices[parent]);
        }
    }
    let (regular_arena, _) = regular.build();

    // Push into the streaming builder in name-sorted order (what the
    // merge in `compact` guarantees), tracking each input index's final
    // position so parents can be patched afterward.
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| ascii::cmp_ci(records[a].0.as_bytes(), records[b].0.as_bytes()));

    let dir = tempfile::tempdir().unwrap();
    let mut streaming = SpooledArenaBuilder::new(records.len(), dir.path(), &|_| {}).unwrap();
    let mut streaming_indices = vec![0u32; records.len()];
    for &i in &order {
        let (name, is_dir, mtime, size, _) = records[i];
        streaming_indices[i] =
            streaming.push(name.as_bytes(), mtime, is_dir, size, true, PARENT_NONE);
    }
    // Patch in an order unrelated to push order, matching the "parent
    // may not have been pushed yet" reality `compact`'s second pass
    // handles.
    for &i in order.iter().rev() {
        if let Some(parent) = records[i].4 {
            streaming.patch_parent(streaming_indices[i], streaming_indices[parent]);
        }
    }
    let columns = streaming.finish();
    let dfs_layout =
        crate::dfs::build_file_backed(columns.parents.as_slice(), dir.path(), &|_| {}).unwrap();
    let exact = derive_spooled_size_exact(
        columns.parents.as_slice(),
        &columns.size_exact_inputs,
        &dfs_layout,
        dir.path(),
        &|_| {},
    )
    .unwrap();
    let dfs_size_prefix = crate::dfs::prefix_sums_u64_file_backed(
        &dfs_layout.records,
        &columns.sizes,
        &dir.path().join("dfs-size-prefix.spool"),
        &|_| {},
    )
    .unwrap();

    assert_eq!(columns.names.as_slice(), regular_arena.names.as_slice());
    assert_eq!(
        columns.bucket_offsets.as_slice(),
        regular_arena.bucket_offsets.as_slice()
    );
    assert_eq!(columns.parents.as_slice(), regular_arena.parents.as_slice());
    assert_eq!(columns.mtimes.as_slice(), regular_arena.mtimes.as_slice());
    assert_eq!(columns.sizes.as_slice(), regular_arena.sizes.as_slice());
    assert_eq!(exact.as_slice(), regular_arena.size_exact_bits.as_slice());
    assert_eq!(
        columns.trigram_index.as_slice(),
        regular_arena.trigram_index.as_slice()
    );
    assert_eq!(
        dfs_layout.positions.as_slice(),
        regular_arena.dfs_positions.as_slice()
    );
    assert_eq!(
        dfs_layout.records.as_slice(),
        regular_arena.dfs_records.as_slice()
    );
    assert_eq!(
        dfs_layout.subtree_ends.as_slice(),
        regular_arena.dfs_ends.as_slice()
    );
    assert_eq!(
        dfs_size_prefix.as_slice(),
        regular_arena.dfs_size_prefix.as_slice()
    );
}

#[test]
fn streaming_builder_matches_across_bucket_and_trigram_boundaries() {
    let count = TRIGRAM_BLOCK + BUCKET_SIZE + 7;
    let records: Vec<_> = (0..count)
        .map(|i| {
            (
                format!("entry_{i:04}_shared_suffix.dat"),
                i as u32,
                (i as u64 + 1) * 1536,
            )
        })
        .collect();

    let mut regular = ArenaBuilder::with_capacity(count, count * 32);
    for (name, mtime, size) in &records {
        regular.push_bytes_with_metadata(name.as_bytes(), *mtime, false, None, *size);
    }
    let (regular_arena, _) = regular.build();

    let dir = tempfile::tempdir().unwrap();
    let mut streaming = SpooledArenaBuilder::new(count, dir.path(), &|_| {}).unwrap();
    for (name, mtime, size) in &records {
        streaming.push(name.as_bytes(), *mtime, false, *size, true, PARENT_NONE);
    }
    let columns = streaming.finish();
    let dfs_layout =
        crate::dfs::build_file_backed(columns.parents.as_slice(), dir.path(), &|_| {}).unwrap();
    let exact = derive_spooled_size_exact(
        columns.parents.as_slice(),
        &columns.size_exact_inputs,
        &dfs_layout,
        dir.path(),
        &|_| {},
    )
    .unwrap();
    let dfs_size_prefix = crate::dfs::prefix_sums_u64_file_backed(
        &dfs_layout.records,
        &columns.sizes,
        &dir.path().join("boundary-prefix.spool"),
        &|_| {},
    )
    .unwrap();

    assert_eq!(columns.names.as_slice(), regular_arena.names.as_slice());
    assert_eq!(
        columns.bucket_offsets.as_slice(),
        regular_arena.bucket_offsets.as_slice()
    );
    assert_eq!(columns.parents.as_slice(), regular_arena.parents.as_slice());
    assert_eq!(columns.mtimes.as_slice(), regular_arena.mtimes.as_slice());
    assert_eq!(columns.sizes.as_slice(), regular_arena.sizes.as_slice());
    assert_eq!(exact.as_slice(), regular_arena.size_exact_bits.as_slice());
    assert_eq!(
        columns.trigram_index.as_slice(),
        regular_arena.trigram_index.as_slice()
    );
    assert_eq!(
        dfs_layout.positions.as_slice(),
        regular_arena.dfs_positions.as_slice()
    );
    assert_eq!(
        dfs_layout.records.as_slice(),
        regular_arena.dfs_records.as_slice()
    );
    assert_eq!(
        dfs_layout.subtree_ends.as_slice(),
        regular_arena.dfs_ends.as_slice()
    );
    assert_eq!(
        dfs_size_prefix.as_slice(),
        regular_arena.dfs_size_prefix.as_slice()
    );
}

/// The key equivalence test for the blob-backed staging refactor: building
/// the same tree via `push` (str) and via `push_bytes` (raw bytes) must
/// produce byte-identical serialized output. That single assertion covers
/// sort order, parent remapping, and front-coding all at once.
#[test]
fn push_bytes_and_push_produce_identical_arenas() {
    let names: Vec<String> = (0..500)
        .map(|i| format!("file_{i:04}_{}.dat", i % 7))
        .collect();

    let mut b1 = ArenaBuilder::default();
    let root1 = b1.push("C:", 0, true);
    for name in &names {
        let idx = b1.push(name, 0, false);
        b1.set_parent(idx, root1);
    }
    let arena1 = b1.build().0;

    let mut b2 = ArenaBuilder::with_capacity(names.len() + 1, 0);
    let root2 = b2.push_bytes(b"C:", 0, true);
    for name in &names {
        let idx = b2.push_bytes(name.as_bytes(), 0, false);
        b2.set_parent(idx, root2);
    }
    let arena2 = b2.build().0;

    let bytes1 = rkyv::to_bytes::<_, 1024>(&arena1).unwrap();
    let bytes2 = rkyv::to_bytes::<_, 1024>(&arena2).unwrap();
    assert_eq!(
        bytes1.as_slice(),
        bytes2.as_slice(),
        "push and push_bytes must produce byte-identical arenas"
    );
}

#[test]
fn builder_with_blob_staging_matches_string_staging() {
    // "String staging" here means the pre-refactor semantics: names
    // pushed one at a time via `push`, compared against a brute-force
    // sort/front-code done independently over owned `String`s. If the
    // blob-backed builder drifted from that behaviour, this test would
    // fail on sort order or front-coding content.
    let names = [
        "readme.txt",
        "Readme.txt",
        "a",
        "abc",
        "abcd",
        "z_last",
        "IMG_0001.jpg",
        "img_0002.jpg",
        "",
        "mid",
    ];

    let arena = simple_arena(&names);

    let mut expected: Vec<&str> = names.to_vec();
    expected.sort_by(|a, b| ascii::cmp_ci(a.as_bytes(), b.as_bytes()));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob_staging.rkyv");
    save(&arena, &path).unwrap();
    let store = crate::store::ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    let actual: Vec<String> = (0..archived.len() as u32)
        .map(|i| archived.name(i))
        .collect();
    assert_eq!(actual, expected);
}
