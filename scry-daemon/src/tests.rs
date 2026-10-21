use scry_core::metrics::QuerySpans;
use scry_core::protocol::{Order, QueryKind};
use scry_core::view::{Hit, SearchOptions};
use scry_core::{ArenaStore, IndexView, Query};
use std::sync::Arc;

use super::*;
use crate::changes::*;
use crate::ffi;
use crate::metrics::*;
use crate::paths::*;
use crate::persistence::*;
use crate::platform::*;
use crate::rpc::*;
use crate::search::*;
use crate::sections::*;
use crate::updates::*;

mod query_probe {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../scry-core/tests/support/query_probe.rs"
    ));
}

fn single_volume(view: Arc<IndexView>) -> VolumeIndexes {
    Arc::new(vec![VolumeIndex {
        volume: "C:".to_string(),
        store: Arc::new(arc_swap::ArcSwap::from(view)),
        cursor: None,
    }])
}

use scry_core::{store::save, Arena};
use scry_fsevents::ChangeEvent;

fn build_store_with_n_records(n: usize, dir: &tempfile::TempDir) -> Arc<IndexView> {
    let mut b = Arena::builder();
    let root = b.push("C:", 0, true);
    for i in 0..n.saturating_sub(1) {
        let child = b.push(&format!("file{i}.txt"), 0, false);
        b.set_parent(child, root);
    }
    let arena = b.build().0;
    let path = dir.path().join(format!("index-{n}.rkyv"));
    save(&arena, &path).unwrap();
    Arc::new(IndexView::new(Arc::new(ArenaStore::open(&path).unwrap())))
}

mod benchmarks;
mod rpc;
mod search;
mod updates;
