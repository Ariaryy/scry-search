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

use scry_core::delta::{ApplyOutcome, DeltaEvent};
use scry_core::protocol::{
    decode_request, encode_results, encode_shared_index, QueryKind, ResultEntry,
    SharedIndexResponse,
};
use scry_core::{ArenaStore, IndexView, Query};
use std::sync::Arc;

/// The live index, published to query threads by atomic pointer swap.
///
/// Readers take a snapshot with a single atomic load and no lock, so a
/// reindex never blocks a query — an in-flight reader keeps its `Arc` alive
/// and continues to see a consistent (if momentarily stale) index while the
/// swap happens underneath it.
type SharedStore = Arc<arc_swap::ArcSwap<IndexView>>;

struct VolumeIndex {
    volume: String,
    store: SharedStore,
    cursor: Option<scry_fsevents::JournalCursor>,
}

struct StartupView {
    view: Arc<IndexView>,
    cursor: Option<scry_fsevents::JournalCursor>,
}

type VolumeIndexes = Arc<Vec<VolumeIndex>>;

struct StartupOptions {
    volumes: Vec<String>,
    index_mbps: Option<u64>,
}

fn startup_options() -> anyhow::Result<StartupOptions> {
    let mut volumes = Vec::new();
    let mut index_mbps = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--unbounded" => index_mbps = Some(0),
            "--index-mbps" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--index-mbps requires a value"))?;
                index_mbps =
                    Some(value.parse().map_err(|_| {
                        anyhow::anyhow!("--index-mbps expects a non-negative integer")
                    })?);
            }
            _ if arg.starts_with("--index-mbps=") => {
                let value = arg.trim_start_matches("--index-mbps=");
                index_mbps =
                    Some(value.parse().map_err(|_| {
                        anyhow::anyhow!("--index-mbps expects a non-negative integer")
                    })?);
            }
            _ if arg.starts_with('-') => return Err(anyhow::anyhow!("unknown option: {arg}")),
            _ => volumes.push(arg),
        }
    }
    Ok(StartupOptions {
        volumes,
        index_mbps,
    })
}

fn main() -> anyhow::Result<()> {
    let options = startup_options()?;
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
    configure_index_read_cap(options.index_mbps);

    let requested_volumes = options.volumes;
    let volume_names = if requested_volumes.is_empty() {
        scry_fsevents::WindowsBackend::fixed_ntfs_volumes()
    } else {
        requested_volumes
    };
    if volume_names.is_empty() {
        return Err(anyhow::anyhow!("no fixed NTFS volumes available"));
    }

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

    let mut indexed = Vec::new();
    for volume in volume_names {
        eprintln!("scryd: indexing {volume}...");
        let initial = match build_or_resume_view(&volume, auxiliary_marking_enabled) {
            Ok(view) => view,
            Err(error) => {
                eprintln!("scryd: skipping {volume}: {error}");
                continue;
            }
        };
        eprintln!("scryd: indexed {} entries on {volume}", initial.view.len());
        indexed.push(VolumeIndex {
            volume,
            store: Arc::new(initial.view.into()),
            cursor: initial.cursor,
        });
    }
    if indexed.is_empty() {
        return Err(anyhow::anyhow!("could not index any requested volume"));
    }
    let indexes: VolumeIndexes = Arc::new(indexed);
    for index in indexes.iter().skip(1) {
        spawn_volume_watcher(
            index.volume.clone(),
            index.store.clone(),
            index.cursor,
            auxiliary_marking_enabled,
        );
    }
    let store = indexes[0].store.clone();
    let volume = indexes[0].volume.clone();
    let cursor = indexes[0].cursor;

    {
        let store = store.clone();
        let volume = volume.clone();
        let (tx, rx) = crossbeam::channel::bounded(16_384);
        let watcher = scry_fsevents::WindowsBackend::spawn_watcher_from(&volume, cursor, tx);
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
                let indexes = indexes.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(pipe, &indexes) {
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
fn spawn_volume_watcher(
    volume: String,
    store: SharedStore,
    cursor: Option<scry_fsevents::JournalCursor>,
    auxiliary_marking_enabled: bool,
) {
    let (tx, rx) = crossbeam::channel::bounded(16_384);
    let watcher = scry_fsevents::WindowsBackend::spawn_watcher_from(&volume, cursor, tx);
    // The daemon owns watchers for its whole process lifetime.
    let watcher: &'static scry_fsevents::JournalHandle = Box::leak(Box::new(watcher));
    std::thread::spawn(move || {
        configure_background_thread_qos();
        reindex_on_changes(volume, rx, store, auxiliary_marking_enabled, watcher)
    });
}

fn configure_index_read_cap(cli_value: Option<u64>) {
    const MIB: u64 = 1024 * 1024;
    let mebibytes_per_second =
        cli_value.unwrap_or_else(|| match std::env::var("SCRY_INDEX_MBPS") {
            Ok(value) => match value.parse::<u64>() {
                Ok(value) => value,
                Err(_) => {
                    eprintln!("scryd: ignoring invalid SCRY_INDEX_MBPS={value:?}; using 128");
                    128
                }
            },
            Err(_) => 128,
        });
    scry_fsevents::configure_index_read_cap(mebibytes_per_second.saturating_mul(MIB));
    eprintln!("scryd: index read cap {mebibytes_per_second} MiB/s");
}

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

fn build_or_resume_view(
    volume: &str,
    auxiliary_marking_enabled: bool,
) -> anyhow::Result<StartupView> {
    match resume_view(volume, auxiliary_marking_enabled) {
        Ok(Some(view)) => {
            eprintln!(
                "scryd: resumed {volume} from journal at {}",
                view.cursor.unwrap().next_usn
            );
            Ok(view)
        }
        Ok(None) => build_view(volume, auxiliary_marking_enabled),
        Err(error) => {
            eprintln!("scryd: full reindex for {volume}: {error}");
            build_view(volume, auxiliary_marking_enabled)
        }
    }
}

fn resume_view(
    volume: &str,
    auxiliary_marking_enabled: bool,
) -> anyhow::Result<Option<StartupView>> {
    let path = snapshot_path(volume);
    if !path.exists() {
        return Ok(None);
    }
    let base = Arc::new(ArenaStore::open(&path)?);
    let archived = base.archived();
    if archived.journal_id == 0 || archived.volume_serial == 0 {
        return Ok(None);
    }
    let stored = scry_fsevents::JournalCursor {
        journal_id: archived.journal_id,
        first_usn: 0,
        next_usn: archived.next_usn,
        volume_serial: archived.volume_serial,
    };
    let (cursor, events) = scry_fsevents::WindowsBackend::replay_journal(volume, stored)
        .map_err(|error| anyhow::anyhow!("cannot replay snapshot: {error}"))?;
    if events.len().saturating_mul(20) > archived.len() {
        return Err(anyhow::anyhow!("replay exceeds the compaction threshold"));
    }

    let mut filter = SelfWriteFilter::new(volume, auxiliary_marking_enabled);
    let mut delta = scry_core::delta::Delta::new(archived.len());
    for change in &events {
        if !is_real_change(change, &mut filter) {
            continue;
        }
        let Some(event) = delta_event(change) else {
            continue;
        };
        if delta.apply(&event, &base) == ApplyOutcome::NeedsFullReindex {
            return Err(anyhow::anyhow!("replay cannot be applied to this snapshot"));
        }
    }
    Ok(Some(StartupView {
        view: Arc::new(IndexView {
            base: base.clone(),
            path_index: Arc::new(scry_core::pathindex::PathIndex::build(archived, &delta)),
            delta: Arc::new(delta),
            generation: scry_core::view::fresh_generation(),
        }),
        cursor: Some(cursor),
    }))
}

fn build_view(volume: &str, auxiliary_marking_enabled: bool) -> anyhow::Result<StartupView> {
    let cursor = scry_fsevents::WindowsBackend::journal_cursor(volume).ok();
    let (mut arena, mut frns) = scry_fsevents::WindowsBackend::bulk_index_volume(volume)
        .map_err(|e| anyhow::anyhow!("indexing {volume} failed: {e}"))?;
    if let Some(cursor) = cursor {
        arena.journal_id = cursor.journal_id;
        arena.next_usn = cursor.next_usn;
        arena.volume_serial = cursor.volume_serial;
    }
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
    Ok(StartupView {
        view: Arc::new(IndexView::new(base)),
        cursor,
    })
}

fn compact_view(
    view: &IndexView,
    volume: &str,
    auxiliary_marking_enabled: bool,
) -> anyhow::Result<Arc<IndexView>> {
    let _background = BackgroundModeGuard::enter();
    let (mut arena, mut frns) = view.compact();
    let archived = view.base.archived();
    arena.journal_id = archived.journal_id;
    arena.next_usn = archived.next_usn;
    arena.volume_serial = archived.volume_serial;
    let path = snapshot_path(volume);
    let volume = volume.to_string();
    let mark = |file: &std::fs::File| {
        if auxiliary_marking_enabled {
            if let Err(error) =
                scry_fsevents::WindowsBackend::mark_handle_as_auxiliary(file, &volume)
            {
                eprintln!("scryd: could not mark compacted index as auxiliary ({error})");
            }
        }
    };
    scry_core::store::save_with_sidecar(&arena, &mut frns, &path, mark, mark)?;
    Ok(Arc::new(IndexView::new(Arc::new(ArenaStore::open(&path)?))))
}

struct BackgroundModeGuard;

impl BackgroundModeGuard {
    fn enter() -> Self {
        unsafe {
            ffi::SetThreadPriority(ffi::GetCurrentThread(), ffi::THREAD_MODE_BACKGROUND_BEGIN);
        }
        Self
    }
}

impl Drop for BackgroundModeGuard {
    fn drop(&mut self) {
        unsafe {
            ffi::SetThreadPriority(ffi::GetCurrentThread(), ffi::THREAD_MODE_BACKGROUND_END);
        }
    }
}

fn trim_working_set() {
    unsafe {
        ffi::mi_collect(true);
        ffi::SetProcessWorkingSetSizeEx(ffi::GetCurrentProcess(), usize::MAX, usize::MAX, 0);
    }
}

fn snapshot_path(volume: &str) -> std::path::PathBuf {
    let safe: String = volume.chars().filter(|c| c.is_alphanumeric()).collect();
    std::path::PathBuf::from(format!("{volume}\\")).join(format!(".scry-index-{safe}.rkyv"))
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
            let next = Arc::new(IndexView {
                base: view.base.clone(),
                path_index: Arc::new(scry_core::pathindex::PathIndex::build(
                    view.base.archived(),
                    &delta,
                )),
                delta: Arc::new(delta),
                generation: scry_core::view::fresh_generation(),
            });
            let changes = next.delta.tombstones.count_ones() as usize + next.delta.added.len();
            if changes.saturating_mul(20) > next.base.archived().len() {
                match compact_view(&next, &volume, auxiliary_marking_enabled) {
                    Ok(compacted) => {
                        let len = compacted.len();
                        store.store(compacted);
                        trim_working_set();
                        eprintln!("scryd: compacted {volume} ({len} entries)");
                    }
                    Err(error) => {
                        store.store(next);
                        eprintln!("scryd: compaction failed: {error}");
                    }
                }
            } else {
                store.store(next);
            }
            continue;
        }

        match build_view(&volume, auxiliary_marking_enabled) {
            Ok(new_view) => {
                let len = new_view.view.len();
                store.store(new_view.view);
                trim_working_set();
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

fn handle_connection(pipe: scry_ipc::Pipe, indexes: &VolumeIndexes) -> std::io::Result<()> {
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
        if req.kind == QueryKind::ShareIndex && indexes.len() != 1 {
            pipe.write_frame(&encode_results(&[]))?;
            continue;
        }
        let snapshot = indexes[0].store.load_full();
        if req.kind == QueryKind::ShareIndex {
            if req.pattern.parse::<u64>().ok() == Some(snapshot.generation) {
                pipe.write_frame(&encode_shared_index(&SharedIndexResponse {
                    handle: 0,
                    len: 0,
                    generation: snapshot.generation,
                    overlay: Vec::new(),
                }))?;
                continue;
            }
            let section = shared_section(&snapshot)?;
            let handle = section.duplicate_for(pipe.client_process_id()?)?;
            pipe.write_frame(&encode_shared_index(&SharedIndexResponse {
                handle,
                len: section.len() as u64,
                generation: snapshot.generation,
                overlay: snapshot.delta.encode_query_overlay(),
            }))?;
            continue;
        }
        let query = match req.kind {
            QueryKind::Prefix => Query::Prefix(req.pattern.clone()),
            QueryKind::Substring => Query::Substring(req.pattern.clone()),
            QueryKind::Wildcard => Query::wildcard(&req.pattern),
            QueryKind::PathTerms => {
                Query::PathTerms(scry_core::terms::parse_terms(&req.pattern).unwrap_or_default())
            }
            QueryKind::ShareIndex => unreachable!(),
        };
        let entries = search_indexes(indexes, &query, req.limit as usize);

        pipe.write_frame(&encode_results(&entries))?;
    }
}

fn search_indexes(indexes: &VolumeIndexes, query: &Query, limit: usize) -> Vec<ResultEntry> {
    if limit == 0 {
        return Vec::new();
    }
    let mut entries: Vec<_> = indexes
        .iter()
        .flat_map(|index| index.store.load_full().search(query, limit))
        .collect();
    entries.sort_by(|left, right| {
        result_rank(query, left)
            .cmp(&result_rank(query, right))
            .then_with(|| left.path.cmp(&right.path))
    });
    entries.truncate(limit);
    entries
}

fn result_rank(query: &Query, entry: &ResultEntry) -> (u8, usize) {
    let name = entry.path.rsplit('\\').next().unwrap_or(&entry.path);
    let quality = match query {
        Query::Prefix(pattern) | Query::Substring(pattern) => {
            if name.eq_ignore_ascii_case(pattern) {
                0
            } else if name
                .get(..pattern.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(pattern))
            {
                1
            } else {
                2
            }
        }
        Query::PathTerms(terms) => {
            let matches_leaf = terms.iter().all(|term| {
                name.as_bytes()
                    .windows(term.len())
                    .any(|part| part.eq_ignore_ascii_case(term.as_bytes()))
            });
            u8::from(!matches_leaf)
        }
        Query::Regex(_) => 2,
    };
    (quality, name.len())
}

fn shared_section(view: &IndexView) -> std::io::Result<Arc<scry_ipc::Section>> {
    use std::sync::{Mutex, OnceLock};
    type SectionCache = Mutex<Option<(usize, Arc<scry_ipc::Section>)>>;
    static CACHE: OnceLock<SectionCache> = OnceLock::new();
    let key = Arc::as_ptr(&view.base) as usize;
    let mut cache = CACHE.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some((cached_key, section)) = &*cache {
        if *cached_key == key {
            return Ok(section.clone());
        }
    }
    let section = Arc::new(scry_ipc::Section::from_file(view.base.snapshot_file())?);
    *cache = Some((key, section.clone()));
    Ok(section)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_local_matches_search_rpc() {
        let dir = tempfile::tempdir().unwrap();
        let view = build_store_with_n_records(200, &dir);
        let expected = view.search(&Query::Substring("file".into()), 50);
        let indexes = Arc::new(vec![VolumeIndex {
            volume: "C:".to_string(),
            store: Arc::new(arc_swap::ArcSwap::from(view)),
            cursor: None,
        }]);
        let pipe_name = format!(
            r"\\.\pipe\scry-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
        let server_indexes = indexes.clone();
        let thread = std::thread::spawn(move || {
            let pipe = server.accept().unwrap();
            handle_connection(pipe, &server_indexes).unwrap();
        });
        let mut client = (0..100)
            .find_map(|_| {
                scry_client::Client::connect_to(&pipe_name)
                    .ok()
                    .or_else(|| {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        None
                    })
            })
            .expect("test pipe did not become ready");
        let actual = client
            .search_local(QueryKind::Substring, "file", 50)
            .unwrap();
        assert_eq!(actual, expected);
        let cached = client
            .search_local(QueryKind::Substring, "file", 50)
            .unwrap();
        assert_eq!(cached, expected);
        drop(client);
        thread.join().unwrap();
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
    /// This pins the atomic publication guarantee relied on here.
    #[test]
    fn concurrent_readers_see_a_consistent_index_across_a_swap() {
        let dir = tempfile::tempdir().unwrap();
        let store_a = build_store_with_n_records(2, &dir);
        let store_b = build_store_with_n_records(3, &dir);

        let swap: SharedStore = Arc::new(store_a.clone().into());

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let swap = swap.clone();
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        let snap = swap.load_full();
                        let len = snap.len();
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

        let swap: SharedStore = Arc::new(store_a.into());
        let snapshot = swap.load_full();
        assert_eq!(snapshot.len(), 2);

        swap.store(store_b);
        // snapshot still points to store A — must still report 2.
        assert_eq!(snapshot.len(), 2);
    }
}
