use super::{generate, synthetic};
use scry_core::{Arena, ArenaStore, Query};

#[test]
fn exhaustive_counts_match_core_without_top_k_truncation_and_generation_is_private() {
    let dir = tempfile::tempdir().unwrap();
    let mut builder = Arena::builder();
    let root = builder.push("root", 0, true);
    let branch = builder.push("branch group", 0, true);
    builder.set_parent(branch, root);
    for i in 0..150 {
        let record = builder.push(&format!("archive_report_{i}.txt"), 0, false);
        builder.set_parent(record, branch);
    }
    let path = dir.path().join("test.rkyv");
    scry_core::store::save(&builder.build().0, &path).unwrap();
    let store = std::sync::Arc::new(ArenaStore::open(&path).unwrap());
    let view = scry_core::IndexView::new(store.clone());
    assert!(generate::count(&store, "wildcard", "*", dir.path()).is_err());
    assert!(generate::count(&store, "path_terms", "ext:txt size:>1kb", dir.path()).is_err());
    assert!(generate::count(&store, "path_terms", "type:file report", dir.path()).is_err());
    for (kind, pattern, query) in [
        ("prefix", "arch", Query::Prefix("arch".into())),
        ("substring", "report", Query::Substring("report".into())),
        (
            "path_terms",
            "\"branch group\" report",
            scry_core::terms::parse_query("\"branch group\" report", 0).unwrap(),
        ),
        (
            "path_terms",
            "ext:txt",
            scry_core::terms::parse_query("ext:txt", 0).unwrap(),
        ),
        ("substring", "absent", Query::Substring("absent".into())),
    ] {
        assert_eq!(
            generate::count(&store, kind, pattern, dir.path()).unwrap() as usize,
            view.search_with(
                &query,
                scry_core::view::SearchOptions::ordered(1000, scry_client::Order::Relevance)
            )
            .len()
        );
    }
    let a = generate::build(&store, 42, 16, dir.path()).unwrap();
    let b = generate::build(&store, 42, 16, dir.path()).unwrap();
    assert_eq!(a, b);
    let workload: crate::workload::Workload = serde_json::from_value(a.0.clone()).unwrap();
    workload.validate().unwrap();
    assert!(a.1["cases"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["matches"] == 150 && c["bucket"] == "lt1k"));
    for secret in ["archive_report", "branch group", ".txt"] {
        assert!(!a.1.to_string().contains(secret));
    }
    assert!(!a.2["variants"].as_array().unwrap().is_empty());
    assert_eq!(a.0["generation"]["version"], 2);
    assert!(a.1["cases"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["shape"] == "negative-substring" && c["matches"] == 0));
    assert!(a.1["cases"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["shape"] == "broad-path"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    for (n, b) in [
        (0, "lt10"),
        (9, "lt10"),
        (10, "lt1k"),
        (999, "lt1k"),
        (1000, "lt100k"),
        (99999, "lt100k"),
        (100000, "ge100k"),
    ] {
        assert_eq!(generate::bucket(n), b);
    }
}

#[test]
fn calibrated_snapshot_is_byte_reproducible_sorted_and_has_intended_diversity() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.rkyv");
    let b = dir.path().join("b.rkyv");
    synthetic::build(&a, 10000, 42, dir.path()).unwrap();
    synthetic::build(&b, 10000, 42, dir.path()).unwrap();
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    let store = ArenaStore::open(&a).unwrap();
    assert_eq!(store.archived().len(), 10000);
    let mut prev = Vec::new();
    store.archived().for_each_name(|_, name| {
        assert!(prev.as_slice() <= name);
        prev.clear();
        prev.extend_from_slice(name);
        std::ops::ControlFlow::Continue(())
    });
    let fp = crate::fingerprint::compute(&store, dir.path()).unwrap();
    assert!(
        (fp["names"]["distinct_ascii_ci_name_ratio"]
            .as_f64()
            .unwrap()
            - 0.328)
            .abs()
            < 0.001
    );
    assert!((10..=12).contains(&fp["shape"]["depth"]["p50"].as_u64().unwrap()));
    assert!((13..=15).contains(&fp["shape"]["depth"]["p90"].as_u64().unwrap()));
    assert!(
        (fp["extensions"]["top10_share_of_all_files"]
            .as_f64()
            .unwrap()
            - 0.6256)
            .abs()
            < 0.06
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}
