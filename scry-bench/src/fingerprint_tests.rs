use super::fingerprint::compute;
use scry_core::{Arena, ArenaStore};

#[test]
fn fingerprint_has_exact_aggregate_shape_names_extensions_and_no_private_text() {
    let dir = tempfile::tempdir().unwrap();
    let mut builder = Arena::builder();
    let root = builder.push("root", 0, true);
    let branch = builder.push("branch", 0, true);
    builder.set_parent(branch, root);
    for (name, parent) in [
        ("REPORT.txt", branch),
        ("report.TXT", root),
        (".hidden", root),
        ("café.bin", root),
    ] {
        let record = builder.push(name, 0, false);
        builder.set_parent(record, parent);
    }
    let path = dir.path().join("fixture.rkyv");
    scry_core::store::save(&builder.build().0, &path).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let store = ArenaStore::open(&path).unwrap();
    let report = compute(&store, scratch.path()).unwrap();
    assert_eq!(report["size"]["records"], 6);
    assert_eq!(report["size"]["directories"], 2);
    assert_eq!(report["shape"]["depth"]["p50"], 1);
    assert_eq!(report["shape"]["depth"]["max"], 2);
    assert_eq!(report["shape"]["directory_fanout"]["p50"], 1);
    assert_eq!(report["shape"]["directory_fanout"]["max"], 4);
    assert_eq!(report["shape"]["largest_nonroot_subtree_records"], 2);
    assert_eq!(report["names"]["distinct_ascii_ci_names"], 5);
    assert_eq!(report["extensions"]["distinct_ascii_ci_extensions"], 2);
    assert_eq!(report["extensions"]["top10_file_count"], 3);
    assert_eq!(report["extensions"]["extensionless_files"], 1);
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    let text = report.to_string();
    for name in ["REPORT.txt", "report.TXT", ".hidden", "café.bin"] {
        assert!(!text.contains(name));
    }
    assert_eq!(report, compute(&store, scratch.path()).unwrap());
}

#[test]
fn empty_and_deep_forests_have_defined_fingerprints_without_recursive_walks() {
    for n in [0, 5_000] {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = Arena::builder();
        let mut parent = None;
        for _ in 0..n {
            let record = builder.push("branch", 0, true);
            if let Some(parent) = parent {
                builder.set_parent(record, parent);
            }
            parent = Some(record);
        }
        let path = dir.path().join("fixture.rkyv");
        scry_core::store::save(&builder.build().0, &path).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let report = compute(&ArenaStore::open(&path).unwrap(), scratch.path()).unwrap();
        if n == 0 {
            assert!(report["shape"]["depth"]["max"].is_null());
        } else {
            assert_eq!(report["shape"]["depth"]["max"], n - 1);
            assert_eq!(report["shape"]["largest_nonroot_subtree_records"], n - 1);
            assert_eq!(report["names"]["distinct_ascii_ci_names"], 1);
        }
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }
}

#[test]
fn snapshot_identity_follows_the_open_file_across_atomic_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.rkyv");
    let replacement = dir.path().join("replacement.rkyv");
    let mut original = Arena::builder();
    original.push("archive", 0, true);
    scry_core::store::save(&original.build().0, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    let identity = super::fingerprint::sha256_snapshot(&store).unwrap();
    let mut changed = Arena::builder();
    changed.push("documents", 0, true);
    scry_core::store::save(&changed.build().0, &replacement).unwrap();
    std::fs::rename(replacement, &path).unwrap();
    assert_eq!(
        super::fingerprint::sha256_snapshot(&store).unwrap(),
        identity
    );
    assert_ne!(
        super::fingerprint::sha256_snapshot(&ArenaStore::open(&path).unwrap()).unwrap(),
        identity
    );
}
