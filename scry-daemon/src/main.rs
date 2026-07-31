//! scryd: background indexing daemon. Bulk-indexes an NTFS volume via
//! scry-fsevents, serves queries over a named pipe, and keeps the index
//! current by watching the USN journal.
//!
//! Update strategy for v1: changes are coalesced (debounced) and applied by
//! a full re-index + snapshot swap, not an incremental patch to the live
//! Arena. rkyv's archived view is read-only by design (that's what makes the
//! mmap zero-copy), so an in-place patch needs a separate mutable overlay
//! layered on top of the base snapshot — that's the natural next step once
//! reindex latency on large volumes stops being acceptable.

use scry_core::protocol::{decode_request, encode_results, QueryKind, ResultEntry};
use scry_core::{ArenaStore, Query};
use std::sync::{Arc, RwLock};

type SharedStore = Arc<RwLock<Arc<ArenaStore>>>;

fn main() -> anyhow::Result<()> {
    let volume = std::env::args().nth(1).unwrap_or_else(|| "C:".to_string());

    eprintln!("scryd: indexing {volume}...");
    let initial = build_store(&volume)?;
    eprintln!("scryd: indexed {} entries", initial.archived().len());
    let store: SharedStore = Arc::new(RwLock::new(initial));

    {
        let store = store.clone();
        let volume = volume.clone();
        let (tx, rx) = crossbeam::channel::unbounded();
        // Leaked intentionally: the watcher runs for the daemon's whole
        // lifetime, same as the pipe server loop below never returning.
        std::mem::forget(scry_fsevents::WindowsBackend::spawn_watcher(&volume, tx));
        std::thread::spawn(move || reindex_on_changes(volume, rx, store));
    }

    eprintln!("scryd: listening on {}", scry_ipc::PIPE_NAME);
    let server = scry_ipc::PipeServer::new(scry_ipc::PIPE_NAME)?;
    loop {
        match server.accept() {
            Ok(pipe) => {
                let store = store.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(pipe, &store) {
                        eprintln!("scryd: connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("scryd: accept error: {e}"),
        }
    }
}

fn build_store(volume: &str) -> anyhow::Result<Arc<ArenaStore>> {
    let arena = scry_fsevents::WindowsBackend::bulk_index_volume(volume)
        .map_err(|e| anyhow::anyhow!("indexing {volume} failed: {e}"))?;
    let path = snapshot_path(volume);
    scry_core::store::save(&arena, &path)?;
    Ok(Arc::new(ArenaStore::open(&path)?))
}

fn snapshot_path(volume: &str) -> std::path::PathBuf {
    let safe: String = volume.chars().filter(|c| c.is_alphanumeric()).collect();
    std::env::temp_dir().join(format!("scry-index-{safe}.rkyv"))
}

fn reindex_on_changes(
    volume: String,
    rx: crossbeam::channel::Receiver<scry_fsevents::ChangeEvent>,
    store: SharedStore,
) {
    use scry_fsevents::ChangeEvent;

    // The snapshot file itself lives on the volume being watched, so writing
    // it produces USN events that would otherwise feed straight back into
    // this function — an infinite reindex loop that never settles. Filter
    // those out by name (for Created/Renamed) and remember the FRNs they
    // land on so the nameless Modified/Deleted variants can be filtered too.
    let path = snapshot_path(&volume);
    let tmp_path = path.with_extension("tmp");
    let snapshot_name = path.file_name().unwrap().to_string_lossy().into_owned();
    let snapshot_tmp_name = tmp_path.file_name().unwrap().to_string_lossy().into_owned();
    let is_own_name = |name: &str| name == snapshot_name || name == snapshot_tmp_name;
    let mut self_frns: std::collections::HashSet<u64> = std::collections::HashSet::new();

    let is_real_change = |ev: &ChangeEvent, self_frns: &mut std::collections::HashSet<u64>| match ev
    {
        ChangeEvent::Created { frn, name, .. } | ChangeEvent::Renamed { frn, name, .. } => {
            if is_own_name(name) {
                self_frns.insert(*frn);
                false
            } else {
                true
            }
        }
        // A data/metadata write cannot change the arena's shape: the index
        // stores names, parent links and mtime only, and mtime isn't
        // queryable. Reindexing on Modified is what made the daemon rebuild
        // continuously on an active volume.
        ChangeEvent::Modified { .. } => false,
        ChangeEvent::Deleted { frn } => {
            let was_self = self_frns.remove(frn);
            !was_self
        }
    };

    loop {
        // Block until something changes...
        let Ok(first) = rx.recv() else {
            eprintln!("scryd: journal watcher channel closed, live updates stopped");
            return;
        };
        let mut triggered = is_real_change(&first, &mut self_frns);

        // ...then absorb a short burst of further changes before paying for
        // a full reindex, so e.g. extracting a zip doesn't trigger thousands
        // of back-to-back rebuilds.
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(500)) {
            triggered |= is_real_change(&ev, &mut self_frns);
        }

        if !triggered {
            continue;
        }

        match build_store(&volume) {
            Ok(new_store) => {
                let len = new_store.archived().len();
                *store.write().unwrap() = new_store;
                eprintln!("scryd: reindexed {volume} ({len} entries)");
            }
            Err(e) => eprintln!("scryd: reindex failed: {e}"),
        }
    }
}

fn handle_connection(pipe: scry_ipc::Pipe, store: &SharedStore) -> std::io::Result<()> {
    loop {
        let req_bytes = match pipe.read_frame() {
            Ok(b) => b,
            Err(_) => return Ok(()), // client disconnected
        };
        let Some(req) = decode_request(&req_bytes) else {
            continue;
        };

        let snapshot = store.read().unwrap().clone();
        let archived = snapshot.archived();
        let query = match req.kind {
            QueryKind::Prefix => Query::Prefix(req.pattern.clone()),
            QueryKind::Substring => Query::Substring(req.pattern.clone()),
            QueryKind::Wildcard => Query::wildcard(&req.pattern),
        };
        let hits = scry_core::query::search(archived, &query, req.limit as usize);
        let entries: Vec<ResultEntry> = hits
            .iter()
            .map(|&idx| {
                let rec = &archived.records[idx as usize];
                ResultEntry {
                    path: archived.full_path(idx, '\\'),
                    size: rec.size,
                    is_dir: rec.is_dir(),
                }
            })
            .collect();

        pipe.write_frame(&encode_results(&entries))?;
    }
}
