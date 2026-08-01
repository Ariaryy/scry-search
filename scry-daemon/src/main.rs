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

mod ffi;

/// The Windows default heap does not return large freed spans to the OS
/// promptly, which is why a reindex spike leaves RSS elevated long after the
/// allocation is gone. mimalloc purges on a timer instead.
#[cfg(windows)]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use arc_swap::ArcSwap;
use scry_core::delta::{ApplyOutcome, DeltaEvent};
use scry_core::protocol::{decode_request, encode_results, QueryKind, ResultEntry};
use scry_core::{ArenaStore, IndexView, Query};
use std::sync::Arc;

/// The live index, published to query threads by atomic pointer swap.
///
/// Readers take a snapshot with a single atomic load and no lock, so a
/// reindex never blocks a query — an in-flight reader keeps its `Arc` alive
/// and continues to see a consistent (if momentarily stale) index while the
/// swap happens underneath it.
type SharedStore = Arc<ArcSwap<IndexView>>;

fn main() -> anyhow::Result<()> {
    // Return freed spans to the OS after 1s of idleness rather than mimalloc's
    // 10s default; this daemon is idle far more often than it is busy.
    if std::env::var_os("MIMALLOC_PURGE_DELAY").is_none() {
        // SAFETY: single-threaded at this point; no other thread reads the env.
        #[allow(deprecated)]
        unsafe {
            std::env::set_var("MIMALLOC_PURGE_DELAY", "1000");
        }
    }

    configure_background_qos();

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
    let initial = build_view(&volume, auxiliary_marking_enabled)?;
    eprintln!("scryd: indexed {} entries", initial.len());
    let store: SharedStore = Arc::new(ArcSwap::from(initial));

    {
        let store = store.clone();
        let volume = volume.clone();
        let (tx, rx) = crossbeam::channel::bounded(16_384);
        let watcher = scry_fsevents::WindowsBackend::spawn_watcher(&volume, tx);
        // Leaked intentionally: the watcher runs for the daemon's whole
        // lifetime, same as the pipe server loop below never returning.
        let watcher: &'static scry_fsevents::JournalHandle = Box::leak(Box::new(watcher));
        std::thread::spawn(move || {
            configure_background_thread_qos();
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

/// Configure this process as a well-behaved background service:
///
/// - **EcoQoS** (`ProcessPowerThrottling` / `EXECUTION_SPEED`): tells the
///   Windows scheduler to prefer efficiency cores and lower clocks. A file
///   indexer has no latency requirement that justifies boost clocks, and this
///   is the single largest lever on the daemon's power draw.
/// - **Low memory priority**: the index is a cache that can be re-faulted from
///   the snapshot file, so under memory pressure these pages should be
///   reclaimed before a foreground app's.
///
/// Both are best-effort. Failures are logged and ignored — an older Windows
/// build simply doesn't support them, and that is not a reason to refuse to
/// run.
fn configure_background_qos() {
    use std::mem::size_of;

    // SAFETY: all pointers point to local structs with the correct layout;
    // the Win32 functions are documented to read only within the supplied size.
    unsafe {
        // EcoQoS: prefer efficiency cores / lower frequencies.
        let mut throttle = ffi::ProcessPowerThrottlingState {
            version: ffi::PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            control_mask: ffi::POWER_THROTTLING_EXECUTION_SPEED,
            state_mask: ffi::POWER_THROTTLING_EXECUTION_SPEED,
        };
        let result = ffi::SetProcessInformation(
            ffi::GetCurrentProcess(),
            ffi::PROCESS_POWER_THROTTLING,
            &mut throttle as *mut _ as *mut std::ffi::c_void,
            size_of::<ffi::ProcessPowerThrottlingState>() as ffi::Dword,
        );
        if result == 0 {
            let code = ffi::GetLastError();
            eprintln!("scryd: EcoQoS unavailable (win32 error {code}); continuing at default QoS");
        }

        // Low memory priority: reclaim daemon pages first under pressure.
        let mut mem_prio = ffi::MemoryPriorityInformation {
            memory_priority: ffi::MEMORY_PRIORITY_LOW,
        };
        let result = ffi::SetProcessInformation(
            ffi::GetCurrentProcess(),
            ffi::PROCESS_MEMORY_PRIORITY,
            &mut mem_prio as *mut _ as *mut std::ffi::c_void,
            size_of::<ffi::MemoryPriorityInformation>() as ffi::Dword,
        );
        if result == 0 {
            let code = ffi::GetLastError();
            eprintln!("scryd: low memory priority unavailable (win32 error {code}); continuing");
        }
    }
}

/// Apply EcoQoS to the calling thread. Must be called from the thread being
/// throttled (self-application avoids handle lifetime issues). Only apply to
/// the reindex worker — query-serving threads must stay responsive.
fn configure_background_thread_qos() {
    use std::mem::size_of;

    // SAFETY: same as configure_background_qos.
    unsafe {
        let mut throttle = ffi::ThreadPowerThrottlingState {
            version: ffi::THREAD_POWER_THROTTLING_CURRENT_VERSION,
            control_mask: ffi::POWER_THROTTLING_EXECUTION_SPEED,
            state_mask: ffi::POWER_THROTTLING_EXECUTION_SPEED,
        };
        let result = ffi::SetThreadInformation(
            ffi::GetCurrentThread(),
            ffi::THREAD_POWER_THROTTLING,
            &mut throttle as *mut _ as *mut std::ffi::c_void,
            size_of::<ffi::ThreadPowerThrottlingState>() as ffi::Dword,
        );
        if result == 0 {
            let code = ffi::GetLastError();
            eprintln!(
                "scryd: thread EcoQoS unavailable (win32 error {code}); reindex thread at default QoS"
            );
        }
    }
}

fn build_view(volume: &str, auxiliary_marking_enabled: bool) -> anyhow::Result<Arc<IndexView>> {
    let (arena, mut frns) = scry_fsevents::WindowsBackend::bulk_index_volume(volume)
        .map_err(|e| anyhow::anyhow!("indexing {volume} failed: {e}"))?;
    let path = snapshot_path(volume);
    let volume = volume.to_string();
    let mark = |f: &std::fs::File| {
        if auxiliary_marking_enabled {
            if let Err(e) = scry_fsevents::WindowsBackend::mark_handle_as_auxiliary(f, &volume) {
                eprintln!("scryd: could not mark index handle as auxiliary ({e})");
            }
        }
    };
    scry_core::store::save_with_sidecar(&arena, &mut frns, &path, mark, mark)?;
    let base = Arc::new(ArenaStore::open(&path)?);
    Ok(Arc::new(IndexView::new(base)))
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
    sidecar_name: String,
    sidecar_tmp_name: String,
    self_frns: std::collections::HashSet<u64>,
    use_auxiliary: bool,
}

impl SelfWriteFilter {
    fn new(volume: &str, use_auxiliary: bool) -> Self {
        let path = snapshot_path(volume);
        let tmp_path = path.with_extension("tmp");
        let sidecar_path = path.with_extension("frn");
        let sidecar_tmp_path = path.with_extension("frn.tmp");
        SelfWriteFilter {
            snapshot_name: path.file_name().unwrap().to_string_lossy().into_owned(),
            snapshot_tmp_name: tmp_path.file_name().unwrap().to_string_lossy().into_owned(),
            sidecar_name: sidecar_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            sidecar_tmp_name: sidecar_tmp_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            self_frns: std::collections::HashSet::new(),
            use_auxiliary,
        }
    }

    fn is_own_name(&self, name: &str) -> bool {
        name == self.snapshot_name
            || name == self.snapshot_tmp_name
            || name == self.sidecar_name
            || name == self.sidecar_tmp_name
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
        let mut batch = Vec::new();
        if is_real_change(&first, &mut filter) {
            batch.push(first);
        }

        // ...then absorb a short burst of further changes before paying for
        // a full reindex, so e.g. extracting a zip doesn't trigger thousands
        // of back-to-back rebuilds.
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(500)) {
            if is_real_change(&ev, &mut filter) {
                batch.push(ev);
            }
        }

        // The channel filled up while we were mid-reindex; a structural
        // event may have been dropped, so force a resync regardless of what
        // the drained events looked like.
        let mut needs_full_reindex = watcher.take_overflow();

        if batch.is_empty() && !needs_full_reindex {
            continue;
        }

        let view = store.load_full();
        let mut delta = (*view.delta).clone();
        for event in &batch {
            let Some(event) = delta_event(event) else {
                continue;
            };
            if delta.apply(&event, &view.base) == ApplyOutcome::NeedsFullReindex {
                needs_full_reindex = true;
                break;
            }
        }

        if !needs_full_reindex {
            store.store(Arc::new(IndexView {
                base: view.base.clone(),
                delta: Arc::new(delta),
            }));
            continue;
        }

        match build_view(&volume, auxiliary_marking_enabled) {
            Ok(new_view) => {
                let len = new_view.len();
                store.store(new_view);
                eprintln!("scryd: reindexed {volume} ({len} entries)");
            }
            Err(e) => eprintln!("scryd: reindex failed: {e}"),
        }
    }
}

fn delta_event(event: &scry_fsevents::ChangeEvent) -> Option<DeltaEvent> {
    use scry_fsevents::ChangeEvent;

    match event {
        ChangeEvent::Created {
            frn,
            parent_frn,
            name,
            is_dir,
            ..
        } => Some(DeltaEvent::Created {
            frn: *frn,
            parent_frn: *parent_frn,
            name: name.clone(),
            is_dir: *is_dir,
            mtime_secs: 0,
        }),
        ChangeEvent::Deleted { frn, .. } => Some(DeltaEvent::Deleted { frn: *frn }),
        ChangeEvent::Renamed {
            frn,
            parent_frn,
            name,
            ..
        } => Some(DeltaEvent::Renamed {
            frn: *frn,
            parent_frn: *parent_frn,
            name: name.clone(),
        }),
        ChangeEvent::Modified { .. } => None,
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

        // `load_full` clones the Arc, keeping this index alive for the whole
        // query even if the reindex thread swaps in a new one mid-search.
        let snapshot = store.load_full();
        let query = match req.kind {
            QueryKind::Prefix => Query::Prefix(req.pattern.clone()),
            QueryKind::Substring => Query::Substring(req.pattern.clone()),
            QueryKind::Wildcard => Query::wildcard(&req.pattern),
        };
        let entries: Vec<ResultEntry> = snapshot.search(&query, req.limit as usize);

        pipe.write_frame(&encode_results(&entries))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scry_core::{store::save, Arena};
    use scry_fsevents::ChangeEvent;

    fn build_store_with_n_records(n: usize, dir: &tempfile::TempDir) -> Arc<ArenaStore> {
        let mut b = Arena::builder();
        let root = b.push("C:", 0, true);
        for i in 0..n.saturating_sub(1) {
            let child = b.push(&format!("file{i}.txt"), 0, false);
            b.set_parent(child, root);
        }
        let arena = b.build().0;
        let path = dir.path().join(format!("index-{n}.rkyv"));
        save(&arena, &path).unwrap();
        Arc::new(ArenaStore::open(&path).unwrap())
    }

    #[test]
    fn modified_events_never_trigger_reindex() {
        let mut filter = SelfWriteFilter::new("C:", false);

        let modified = ChangeEvent::Modified {
            frn: 1,
            is_auxiliary: false,
        };
        assert!(!is_real_change(&modified, &mut filter));

        let auxiliary_created = ChangeEvent::Created {
            frn: 2,
            parent_frn: 0,
            name: "not-the-snapshot.txt".to_string(),
            is_dir: false,
            is_auxiliary: true,
        };
        let mut aux_filter = SelfWriteFilter::new("C:", true);
        assert!(!is_real_change(&auxiliary_created, &mut aux_filter));

        let real_created = ChangeEvent::Created {
            frn: 3,
            parent_frn: 0,
            name: "not-the-snapshot.txt".to_string(),
            is_dir: false,
            is_auxiliary: false,
        };
        assert!(is_real_change(&real_created, &mut filter));
    }

    /// Struct sizes must match their Win32 counterparts exactly — a mismatch
    /// causes `SetProcessInformation` to fail with ERROR_INVALID_PARAMETER.
    #[test]
    fn power_throttling_state_is_24_bytes_or_less() {
        assert_eq!(std::mem::size_of::<ffi::ProcessPowerThrottlingState>(), 12);
        assert_eq!(std::mem::size_of::<ffi::MemoryPriorityInformation>(), 4);
    }

    /// `configure_background_qos` must not panic; failures are logged and
    /// ignored by design (older Windows builds may not support EcoQoS).
    #[test]
    fn configure_background_qos_does_not_panic() {
        configure_background_qos();
    }

    /// Four reader threads each observe `archived().len()` is either 2 or 3
    /// (never corrupted) while the main thread swaps between two stores.
    /// This pins the core `ArcSwap` correctness guarantee relied on here.
    #[test]
    fn concurrent_readers_see_a_consistent_index_across_a_swap() {
        let dir = tempfile::tempdir().unwrap();
        let store_a = build_store_with_n_records(2, &dir);
        let store_b = build_store_with_n_records(3, &dir);

        let swap: Arc<ArcSwap<ArenaStore>> = Arc::new(ArcSwap::from(store_a.clone()));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let swap = swap.clone();
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        let snap = swap.load_full();
                        let len = snap.archived().len();
                        assert!(
                            len == 2 || len == 3,
                            "unexpected len {len} — index corrupted during swap"
                        );
                    }
                })
            })
            .collect();

        for _ in 0..1_000 {
            swap.store(store_b.clone());
            swap.store(store_a.clone());
        }

        for h in handles {
            h.join().expect("reader thread panicked");
        }
    }

    /// A `load_full` snapshot survives the store being replaced: callers that
    /// hold a snapshot mid-search continue to see a consistent, valid index
    /// even after a reindex swaps in a new one.
    #[test]
    fn load_full_survives_the_store_being_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let store_a = build_store_with_n_records(2, &dir);
        let store_b = build_store_with_n_records(3, &dir);

        let swap: Arc<ArcSwap<ArenaStore>> = Arc::new(ArcSwap::from(store_a));
        let snapshot = swap.load_full();
        assert_eq!(snapshot.archived().len(), 2);

        swap.store(store_b);
        // snapshot still points to store A — must still report 2.
        assert_eq!(snapshot.archived().len(), 2);
    }
}
