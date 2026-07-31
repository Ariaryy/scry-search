use crate::arena::Arena;
use crate::query::{search, Query};
use crate::record::{EntryFlags, FileRecord};
use crate::store::{save, ArenaStore};

fn sample_arena() -> Arena {
    let mut b = Arena::builder();
    let root = b.push(FileRecord {
        parent: u32::MAX,
        name: "C:".into(),
        size: 0,
        mtime: 0,
        flags: EntryFlags::Directory,
    });
    let docs = b.push(FileRecord {
        parent: root,
        name: "Documents".into(),
        size: 0,
        mtime: 0,
        flags: EntryFlags::Directory,
    });
    b.push(FileRecord {
        parent: docs,
        name: "report.docx".into(),
        size: 1234,
        mtime: 0,
        flags: EntryFlags::File,
    });
    b.push(FileRecord {
        parent: docs,
        name: "report_final.docx".into(),
        size: 5678,
        mtime: 0,
        flags: EntryFlags::File,
    });
    b.push(FileRecord {
        parent: root,
        name: "readme.txt".into(),
        size: 42,
        mtime: 0,
        flags: EntryFlags::File,
    });
    b.build()
}

fn open_sample_store(dir: &tempfile::TempDir) -> ArenaStore {
    let arena = sample_arena();
    let path = dir.path().join("index.rkyv");
    save(&arena, &path).unwrap();
    ArenaStore::open(&path).unwrap()
}

#[test]
fn prefix_search_finds_no_match_for_unknown_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_sample_store(&dir);
    let hits = search(store.archived(), &Query::Prefix("nonsense".into()), 10);
    assert!(hits.is_empty());
}

#[test]
fn round_trip_through_mmap_store_preserves_data() {
    let arena = sample_arena();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.rkyv");
    save(&arena, &path).unwrap();

    let store = ArenaStore::open(&path).unwrap();
    let archived = store.archived();
    assert_eq!(archived.len(), 5);

    let hits = search(archived, &Query::Prefix("report".into()), 10);
    assert_eq!(hits.len(), 2);
    let mut names: Vec<String> = hits
        .iter()
        .map(|&i| archived.records[i as usize].name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["report.docx", "report_final.docx"]);

    let full = archived.full_path(hits[0], '\\');
    assert!(full.starts_with("C:\\Documents\\report"));
}

#[test]
fn wildcard_query_matches_extension() {
    let arena = sample_arena();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.rkyv");
    save(&arena, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    let hits = search(archived, &Query::wildcard("*.docx"), 10);
    assert_eq!(hits.len(), 2);

    let hits = search(archived, &Query::wildcard("readme.*"), 10);
    assert_eq!(hits.len(), 1);
}

#[test]
fn substring_query_is_case_insensitive() {
    let arena = sample_arena();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.rkyv");
    save(&arena, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    let hits = search(archived, &Query::Substring("FINAL".into()), 10);
    assert_eq!(hits.len(), 1);
}
