use crate::arena::Arena;
use crate::query::{search, Query};
use crate::store::{save, ArenaStore};

fn sample_arena() -> Arena {
    let mut b = Arena::builder();
    let root = b.push("C:".to_string(), 0, true);
    let docs = b.push("Documents".to_string(), 0, true);
    b.set_parent(docs, root);
    let r1 = b.push("report.docx".to_string(), 0, false);
    b.set_parent(r1, docs);
    let r2 = b.push("report_final.docx".to_string(), 0, false);
    b.set_parent(r2, docs);
    let readme = b.push("readme.txt".to_string(), 0, false);
    b.set_parent(readme, root);
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
    let mut names: Vec<String> = hits.iter().map(|&i| archived.name(i)).collect();
    names.sort();
    assert_eq!(names, vec!["report.docx", "report_final.docx"]);

    for &hit in &hits {
        let full = archived.full_path(hit, '\\');
        assert!(full.starts_with("C:\\Documents\\report"));
    }
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
