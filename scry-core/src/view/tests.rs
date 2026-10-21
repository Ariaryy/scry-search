use super::path_search::*;
use super::ranked::*;
use super::*;
use crate::delta::{DeltaRecord, ParentRef};
use crate::query::{EntryKind, QueryFilter};
use crate::Arena;
use crate::{ascii, rank};
use regex_automata::{meta::Regex, util::syntax};
use std::collections::BinaryHeap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

#[cfg(windows)]
mod query_probe {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/query_probe.rs"
    ));
}

fn base_view(count: usize) -> (tempfile::TempDir, IndexView) {
    let mut builder = crate::ArenaBuilder::default();
    let root = builder.push("C:", 0, true);
    for i in 0..count {
        let child = builder.push(&format!("match_{i:04}.txt"), 0, false);
        builder.set_parent(child, root);
    }
    let (arena, _) = builder.build();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("view.rkyv");
    crate::store::save(&arena, &path).unwrap();
    let base = Arc::new(ArenaStore::open(&path).unwrap());
    (dir, IndexView::new(base))
}

fn open_compacted(view: &IndexView) -> (tempfile::TempDir, IndexView) {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let path = dir.path().join("compacted.rkyv");
    view.compact_to_snapshot(&scratch, &path, &|_| {}, &mut |_| {})
        .unwrap();
    let base = Arc::new(ArenaStore::open(&path).unwrap());
    (dir, IndexView::new(base))
}

mod benchmarks;
mod compaction;
mod path_terms;
mod ranking;
