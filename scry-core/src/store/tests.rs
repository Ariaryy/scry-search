use crate::record::FORMAT_VERSION;

use super::*;

/// `save_columns_with` must emit exactly the archive `save_with` would
/// for the equivalent owned `Arena` — same header, same rkyv bytes — so
/// this compares full file contents, not just that `ArenaStore::open`
/// accepts the result.
#[test]
fn save_columns_with_matches_save_with_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = crate::arena::ArenaBuilder::default();
    b.push("alpha", 10, true);
    b.push("alpha/beta.txt", 20, false);
    b.push("gamma.txt", 30, false);
    for i in 0..(crate::trigram::TRIGRAM_BLOCK + crate::record::BUCKET_SIZE + 7) {
        b.push(&format!("entry_{i:04}_shared_suffix.dat"), i as u32, false);
    }
    let (mut arena, _) = b.build();
    arena.journal_id = 42;
    arena.next_usn = 7;
    arena.volume_serial = 99;

    let derived_path = dir.path().join("derived.rkyv");
    save(&arena, &derived_path).unwrap();

    let columns = ArenaColumns {
        format_version: arena.format_version,
        journal_id: arena.journal_id,
        next_usn: arena.next_usn,
        volume_serial: arena.volume_serial,
        names: &arena.names,
        bucket_offsets: &arena.bucket_offsets,
        parents: &arena.parents,
        mtimes: &arena.mtimes,
        sizes: &arena.sizes,
        size_exact_bits: &arena.size_exact_bits,
        trigram_index: &arena.trigram_index,
        dfs_positions: &arena.dfs_positions,
        dfs_records: &arena.dfs_records,
        dfs_ends: &arena.dfs_ends,
        dfs_size_prefix: &arena.dfs_size_prefix,
    };
    let manual_path = dir.path().join("manual.rkyv");
    save_columns_with(&columns, &manual_path, |_| {}).unwrap();

    assert_eq!(
        std::fs::read(&derived_path).unwrap(),
        std::fs::read(&manual_path).unwrap()
    );

    // And the manually-produced archive must actually validate and read
    // back correctly through the normal open path.
    let store = ArenaStore::open(&manual_path).unwrap();
    let reopened = store.archived();
    assert_eq!(reopened.len(), arena.len());
    let mut name = Vec::new();
    reopened.name_into(1, &mut name);
    assert_eq!(name, b"alpha/beta.txt");
}

/// Replacing the path publishes a new generation without invalidating an
/// mmap held by an in-flight reader. This is the file-level fault case;
/// ArcSwap and client handshake tests cover publication/detection above it.
#[test]
fn live_mapping_survives_atomic_snapshot_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replaceable.rkyv");

    let mut first_builder = crate::arena::ArenaBuilder::default();
    first_builder.push_bytes_with_frn(b"first", 0, false, 1);
    let (first, mut first_frns) = first_builder.build();
    save_with_sidecar(&first, &mut first_frns, &path, |_| {}, |_| {}).unwrap();
    let old_mapping = ArenaStore::open(&path).unwrap();
    let old_tag = u32::from_le_bytes(old_mapping.archive_bytes()[12..16].try_into().unwrap());

    let mut second_builder = crate::arena::ArenaBuilder::default();
    second_builder.push_bytes_with_frn(b"second-a", 0, false, 2);
    second_builder.push_bytes_with_frn(b"second-b", 0, false, 3);
    let (second, mut second_frns) = second_builder.build();
    save_with_sidecar(&second, &mut second_frns, &path, |_| {}, |_| {}).unwrap();
    let new_mapping = ArenaStore::open(&path).unwrap();
    let new_tag = u32::from_le_bytes(new_mapping.archive_bytes()[12..16].try_into().unwrap());

    assert_eq!(old_mapping.archived().len(), 1);
    assert_eq!(old_mapping.archived().name(0), "first");
    assert_eq!(new_mapping.archived().len(), 2);
    assert_ne!(old_tag, new_tag, "replacement must publish a new tag");
}

#[test]
fn open_ignores_a_sidecar_from_another_snapshot_generation() {
    let dir = tempfile::tempdir().unwrap();

    let mut first_builder = crate::arena::ArenaBuilder::default();
    first_builder.push_bytes_with_frn(b"first", 0, false, 11);
    let (first, mut first_frns) = first_builder.build();
    let first_path = dir.path().join("first.rkyv");
    save_with_sidecar(&first, &mut first_frns, &first_path, |_| {}, |_| {}).unwrap();
    assert!(ArenaStore::open(&first_path).unwrap().frn_map.is_some());

    let mut second_builder = crate::arena::ArenaBuilder::default();
    second_builder.push_bytes_with_frn(b"second", 0, false, 22);
    let (second, mut second_frns) = second_builder.build();
    let second_path = dir.path().join("second.rkyv");
    save_with_sidecar(&second, &mut second_frns, &second_path, |_| {}, |_| {}).unwrap();

    std::fs::copy(
        second_path.with_extension("frn"),
        first_path.with_extension("frn"),
    )
    .unwrap();
    let reopened = ArenaStore::open(&first_path).unwrap();
    assert!(reopened.frn_map.is_none());
}

#[test]
fn open_diagnoses_a_stale_or_alien_snapshot_from_the_header() {
    // Save a valid arena, then patch the version in its header.
    let dir = tempfile::tempdir().unwrap();
    let mut b = crate::arena::ArenaBuilder::default();
    b.push("test", 0, false);
    let arena = b.build().0;
    let path = dir.path().join("versioned.rkyv");
    save(&arena, &path).unwrap();

    // The version lives in the header at a fixed offset, so patching it is
    // exact rather than brittle — which is the whole point of the header.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[8..12].copy_from_slice(&(FORMAT_VERSION - 1).to_le_bytes());
    let stale_path = dir.path().join("stale.rkyv");
    std::fs::write(&stale_path, &bytes).unwrap();
    match ArenaStore::open(&stale_path) {
        Err(StoreError::VersionMismatch { found, expected }) => {
            assert_eq!(found, FORMAT_VERSION - 1);
            assert_eq!(expected, FORMAT_VERSION);
        }
        other => panic!(
            "a stale snapshot must report a version mismatch, not a \
             bytecheck failure over a layout that no longer applies; got {:?}",
            other.map(|_| "Ok")
        ),
    }

    // A file that isn't a snapshot at all is rejected on the magic.
    let alien_path = dir.path().join("alien.rkyv");
    std::fs::write(
        &alien_path,
        b"PK\x03\x04 definitely a zip file, not an index",
    )
    .unwrap();
    assert!(matches!(
        ArenaStore::open(&alien_path),
        Err(StoreError::BadMagic)
    ));

    let random_path = dir.path().join("random.rkyv");
    std::fs::write(
        &random_path,
        b"this is not a valid rkyv archive at all xxxx",
    )
    .unwrap();
    let result = ArenaStore::open(&random_path);
    assert!(
        result.is_err(),
        "expected error opening invalid archive, got Ok"
    );
}

#[test]
fn store_opens_without_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("standalone.rkyv");
    let mut builder = crate::ArenaBuilder::default();
    builder.push("file", 0, false);
    save(&builder.build().0, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    assert!(store.frn_map.is_none());
}
