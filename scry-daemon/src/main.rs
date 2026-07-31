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

    // Enables exact self-write identification via FSCTL_MARK_HANDLE. Requires
    // SeManageVolumePrivilege, which an elevated Administrators token holds
    // but doesn't enable by default. Non-fatal: build_store falls back to the
    // name/FRN heuristic if this fails.
    let auxiliary_marking_enabled =
        match scry_fsevents::WindowsBackend::enable_privilege("SeManageVolumePrivilege") {
            Ok(()) => true,
            Err(e) => {
                eprintln!(
                    "scryd: could not enable SeManageVolumePrivilege ({e}), \
                     falling back to name-based self-write detection"
                );
                false
            }
        };

    eprintln!("scryd: indexing {volume}...");
    let initial = build_store(&volume, auxiliary_marking_enabled)?;
    eprintln!("scryd: indexed {} entries", initial.archived().len());
    let store: SharedStore = Arc::new(RwLock::new(initial));

    {
        let store = store.clone();
        let volume = volume.clone();
        let (tx, rx) = crossbeam::channel::bounded(16_384);
        let watcher = scry_fsevents::WindowsBackend::spawn_watcher(&volume, tx);
        // Leaked intentionally: the watcher runs for the daemon's whole
        // lifetime, same as the pipe server loop below never returning.
        let watcher: &'static scry_fsevents::JournalHandle = Box::leak(Box::new(watcher));
        std::thread::spawn(move || {
            reindex_on_changes(volume, rx, store, auxiliary_marking_enabled, watcher)
        });
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

fn build_store(volume: &str, auxiliary_marking_enabled: bool) -> anyhow::Result<Arc<ArenaStore>> {
    let arena = scry_fsevents::WindowsBackend::bulk_index_volume(volume)
        .map_err(|e| anyhow::anyhow!("indexing {volume} failed: {e}"))?;
    let path = snapshot_path(volume);
    let volume = volume.to_string();
    scry_core::store::save_with(&arena, &path, |f| {
        if auxiliary_marking_enabled {
            if let Err(e) = scry_fsevents::WindowsBackend::mark_handle_as_auxiliary(f, &volume) {
                eprintln!("scryd: could not mark snapshot handle as auxiliary ({e})");
            }
        }
    })?;
    Ok(Arc::new(ArenaStore::open(&path)?))
}

fn snapshot_path(volume: &str) -> std::path::PathBuf {
    let safe: String = volume.chars().filter(|c| c.is_alphanumeric()).collect();
    std::env::temp_dir().join(format!("scry-index-{safe}.rkyv"))
}

/// Holds the state `is_real_change` needs to recognize the daemon's own
/// snapshot writes: the name-based fallback set, and whether
/// `FSCTL_MARK_HANDLE` auxiliary marking is active (in which case
/// `is_auxiliary` is trusted over the heuristic).
struct SelfWriteFilter {
    snapshot_name: String,
    snapshot_tmp_name: String,
    self_frns: std::collections::HashSet<u64>,
    use_auxiliary: bool,
}

impl SelfWriteFilter {
    fn new(volume: &str, use_auxiliary: bool) -> Self {
        let path = snapshot_path(volume);
        let tmp_path = path.with_extension("tmp");
        SelfWriteFilter {
            snapshot_name: path.file_name().unwrap().to_string_lossy().into_owned(),
            snapshot_tmp_name: tmp_path.file_name().unwrap().to_string_lossy().into_owned(),
            self_frns: std::collections::HashSet::new(),
            use_auxiliary,
        }
    }

    fn is_own_name(&self, name: &str) -> bool {
        name == self.snapshot_name || name == self.snapshot_tmp_name
    }
}

// The snapshot file itself lives on the volume being watched, so writing it
// produces USN events that would otherwise feed straight back into
// `reindex_on_changes` — an infinite reindex loop that never settles. When
// auxiliary marking is enabled, `is_auxiliary` alone identifies these
// exactly. The name/FRN heuristic stays as a fallback for when
// SeManageVolumePrivilege couldn't be enabled.
fn is_real_change(ev: &scry_fsevents::ChangeEvent, state: &mut SelfWriteFilter) -> bool {
    use scry_fsevents::ChangeEvent;

    if ev.is_auxiliary() && state.use_auxiliary {
        if let ChangeEvent::Deleted { frn, .. } = ev {
            state.self_frns.remove(frn);
        }
        return false;
    }
    match ev {
        ChangeEvent::Created { frn, name, .. } | ChangeEvent::Renamed { frn, name, .. } => {
            if state.is_own_name(name) {
                state.self_frns.insert(*frn);
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
        ChangeEvent::Deleted { frn, .. } => {
            let was_self = state.self_frns.remove(frn);
            !was_self
        }
    }
}

fn reindex_on_changes(
    volume: String,
    rx: crossbeam::channel::Receiver<scry_fsevents::ChangeEvent>,
    store: SharedStore,
    auxiliary_marking_enabled: bool,
    watcher: &scry_fsevents::JournalHandle,
) {
    let mut filter = SelfWriteFilter::new(&volume, auxiliary_marking_enabled);

    loop {
        // Block until something changes...
        let Ok(first) = rx.recv() else {
            eprintln!("scryd: journal watcher channel closed, live updates stopped");
            return;
        };
        let mut triggered = is_real_change(&first, &mut filter);

        // ...then absorb a short burst of further changes before paying for
        // a full reindex, so e.g. extracting a zip doesn't trigger thousands
        // of back-to-back rebuilds.
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(500)) {
            triggered |= is_real_change(&ev, &mut filter);
        }

        // The channel filled up while we were mid-reindex; a structural
        // event may have been dropped, so force a resync regardless of what
        // the drained events looked like.
        triggered |= watcher.take_overflow();

        if !triggered {
            continue;
        }

        match build_store(&volume, auxiliary_marking_enabled) {
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
