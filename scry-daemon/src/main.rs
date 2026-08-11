#![cfg_attr(windows, windows_subsystem = "windows")]

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
use scry_core::metrics::QuerySpans;
use scry_core::protocol::{
    decode_request, encode_results, encode_shared_index, Order, QueryKind, ResultEntry,
    SharedIndexResponse,
};
use scry_core::view::{Hit, SearchOptions};
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

#[derive(Default, Clone, Copy, Debug)]
struct MemorySample {
    private_usage: u64,
    working_set: u64,
    peak_working_set: u64,
    page_faults: u32,
}

struct QueryMetrics {
    count: u64,
    total: QuerySpans,
    max: QuerySpans,
    samples: [QuerySpans; 1024],
    next: usize,
    sample_count: usize,
    memory: MemorySample,
    last_memory: std::time::Instant,
}

impl QueryMetrics {
    fn new() -> Self {
        Self {
            count: 0,
            total: QuerySpans::default(),
            max: QuerySpans::default(),
            samples: [QuerySpans::default(); 1024],
            next: 0,
            sample_count: 0,
            memory: MemorySample::default(),
            last_memory: std::time::Instant::now() - std::time::Duration::from_secs(1),
        }
    }

    fn record(&mut self, spans: QuerySpans) {
        self.count += 1;
        add_spans(&mut self.total, spans);
        max_spans(&mut self.max, spans);
        self.samples[self.next] = spans;
        self.next = (self.next + 1) % self.samples.len();
        self.sample_count = (self.sample_count + 1).min(self.samples.len());
        self.sample_memory(false);
    }

    fn sample_memory(&mut self, force: bool) {
        if force || self.last_memory.elapsed() >= std::time::Duration::from_secs(1) {
            self.memory = process_memory();
            self.last_memory = std::time::Instant::now();
        }
    }

    fn report(&mut self) -> String {
        self.sample_memory(true);
        let mut out = format!("queries: {}\n", self.count);
        for (label, get) in [
            ("select_ns", span_select as fn(QuerySpans) -> u64),
            ("finalize_ns", span_finalize),
            ("merge_ns", span_merge),
            ("materialize_ns", span_materialize),
            ("encode_ns", span_encode),
            ("candidates", span_candidates),
            ("emitted", span_emitted),
            ("blocks_scanned", span_blocks_scanned),
            ("blocks_total", span_blocks_total),
        ] {
            let mut values: Vec<u64> = self.samples[..self.sample_count]
                .iter()
                .copied()
                .map(get)
                .collect();
            values.sort_unstable();
            let p50 = values.get(values.len() / 2).copied().unwrap_or_default();
            let p99 = values
                .get(values.len().saturating_mul(99) / 100)
                .copied()
                .unwrap_or_default();
            out.push_str(&format!(
                "{label}: sum={} max={} p50={p50} p99={p99}\n",
                get(self.total),
                get(self.max)
            ));
        }
        out.push_str(&format!(
            "private_usage={} working_set={} peak_working_set={} page_faults={}",
            self.memory.private_usage,
            self.memory.working_set,
            self.memory.peak_working_set,
            self.memory.page_faults,
        ));
        out
    }
}

static QUERY_METRICS: std::sync::OnceLock<std::sync::Mutex<QueryMetrics>> =
    std::sync::OnceLock::new();

fn query_metrics() -> &'static std::sync::Mutex<QueryMetrics> {
    QUERY_METRICS.get_or_init(|| std::sync::Mutex::new(QueryMetrics::new()))
}

macro_rules! span_fields {
    ($target:expr, $source:expr, $op:expr) => {{
        $op(&mut $target.select_ns, $source.select_ns);
        $op(&mut $target.finalize_ns, $source.finalize_ns);
        $op(&mut $target.merge_ns, $source.merge_ns);
        $op(&mut $target.materialize_ns, $source.materialize_ns);
        $op(&mut $target.encode_ns, $source.encode_ns);
        $op(&mut $target.candidates, $source.candidates);
        $op(&mut $target.emitted, $source.emitted);
        $op(&mut $target.blocks_scanned, $source.blocks_scanned);
        $op(&mut $target.blocks_total, $source.blocks_total);
    }};
}

fn add_spans(target: &mut QuerySpans, source: QuerySpans) {
    span_fields!(target, source, |field: &mut u64, value| *field =
        field.saturating_add(value));
}

fn max_spans(target: &mut QuerySpans, source: QuerySpans) {
    span_fields!(target, source, |field: &mut u64, value| *field =
        (*field).max(value));
}

fn span_select(spans: QuerySpans) -> u64 {
    spans.select_ns
}
fn span_finalize(spans: QuerySpans) -> u64 {
    spans.finalize_ns
}
fn span_merge(spans: QuerySpans) -> u64 {
    spans.merge_ns
}
fn span_materialize(spans: QuerySpans) -> u64 {
    spans.materialize_ns
}
fn span_encode(spans: QuerySpans) -> u64 {
    spans.encode_ns
}
fn span_candidates(spans: QuerySpans) -> u64 {
    spans.candidates
}
fn span_emitted(spans: QuerySpans) -> u64 {
    spans.emitted
}
fn span_blocks_scanned(spans: QuerySpans) -> u64 {
    spans.blocks_scanned
}
fn span_blocks_total(spans: QuerySpans) -> u64 {
    spans.blocks_total
}

fn process_memory() -> MemorySample {
    let mut counters: ffi::ProcessMemoryCountersEx = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<ffi::ProcessMemoryCountersEx>() as u32;
    if unsafe { ffi::GetProcessMemoryInfo(ffi::GetCurrentProcess(), &mut counters, counters.cb) }
        == 0
    {
        return MemorySample::default();
    }
    MemorySample {
        private_usage: counters.PrivateUsage as u64,
        working_set: counters.WorkingSetSize as u64,
        peak_working_set: counters.PeakWorkingSetSize as u64,
        page_faults: counters.PageFaultCount,
    }
}

/// Read by `handle_console_ctrl`, which — being a plain Win32 callback — has
/// no way to receive the indexes as an argument. Set once at startup, never
/// mutated after.
static SHUTDOWN_STATE: std::sync::OnceLock<(VolumeIndexes, bool)> = std::sync::OnceLock::new();

/// Persists every volume's index before the process goes away. Registered
/// via `SetConsoleCtrlHandler` so a console close, logoff, shutdown, or
/// Ctrl+C loses at most the work since the last idle/compaction write instead
/// of everything since the last snapshot. `CLOSE`/`LOGOFF`/`SHUTDOWN` kill the
/// process regardless of the return value, so this exits explicitly rather
/// than returning and hoping the default handler waits for us.
extern "system" fn handle_console_ctrl(ctrl_type: ffi::Dword) -> ffi::Bool {
    if !is_shutdown_signal(ctrl_type) {
        return 0;
    }
    if let Some((indexes, auxiliary_marking_enabled)) = SHUTDOWN_STATE.get() {
        for index in indexes.iter() {
            let _ = persist_idle_view(&index.store, &index.volume, *auxiliary_marking_enabled);
        }
    }
    std::process::exit(0);
}

fn is_shutdown_signal(ctrl_type: ffi::Dword) -> bool {
    matches!(
        ctrl_type,
        ffi::CTRL_C_EVENT
            | ffi::CTRL_BREAK_EVENT
            | ffi::CTRL_CLOSE_EVENT
            | ffi::CTRL_LOGOFF_EVENT
            | ffi::CTRL_SHUTDOWN_EVENT
    )
}

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

    // Every indexed volume's snapshot lives under the same profile directory
    // (see `snapshot_path`), so a daemon indexing several volumes can end up
    // writing all of them onto one physical drive. Each volume's write filter
    // needs the full list to recognize a sibling volume's snapshot file as
    // its own, not just the file it writes itself.
    let all_volumes: std::sync::Arc<Vec<String>> = std::sync::Arc::new(volume_names.clone());

    let mut indexed = Vec::new();
    for volume in volume_names {
        eprintln!("scryd: indexing {volume}...");
        let initial = match build_or_resume_view(&volume, &all_volumes, auxiliary_marking_enabled) {
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
    let _ = SHUTDOWN_STATE.set((indexes.clone(), auxiliary_marking_enabled));
    // SAFETY: `handle_console_ctrl` matches `HandlerRoutine`'s signature and
    // reads only `SHUTDOWN_STATE`, which is set above and never mutated again.
    unsafe {
        if ffi::SetConsoleCtrlHandler(handle_console_ctrl, 1) == 0 {
            eprintln!(
                "scryd: could not register console control handler (win32 error {}); \
                 clean-shutdown persistence disabled",
                ffi::GetLastError()
            );
        }
    }
    for index in indexes.iter().skip(1) {
        spawn_volume_watcher(
            index.volume.clone(),
            all_volumes.clone(),
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
        let all_volumes = all_volumes.clone();
        let (tx, rx) = crossbeam::channel::bounded(16_384);
        let watcher = scry_fsevents::WindowsBackend::spawn_watcher_from(&volume, cursor, tx);
        // Leaked intentionally: the watcher runs for the daemon's whole
        // lifetime, same as the pipe server loop below never returning.
        let watcher: &'static scry_fsevents::JournalHandle = Box::leak(Box::new(watcher));
        std::thread::spawn(move || {
            configure_background_thread_qos();
            reindex_on_changes(
                volume,
                &all_volumes,
                rx,
                store,
                auxiliary_marking_enabled,
                watcher,
            )
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
    all_volumes: std::sync::Arc<Vec<String>>,
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
        reindex_on_changes(
            volume,
            &all_volumes,
            rx,
            store,
            auxiliary_marking_enabled,
            watcher,
        )
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
    all_volumes: &[String],
    auxiliary_marking_enabled: bool,
) -> anyhow::Result<StartupView> {
    match resume_view(volume, all_volumes, auxiliary_marking_enabled) {
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

/// Shared by warm-launch replay and by the live reindex loop: once accrued
/// change events exceed 5% of the base, a streaming compaction is cheaper
/// than the delta staying live indefinitely. The threshold is intentionally
/// not lower: on a 100k-record release corpus, growing the delta from 0% to 5%
/// added only 28 us to a no-match substring query and 21 us to a path-term
/// query, while compaction cost 83-103 ms at 0.5%, 1%, and 5% because it
/// rewrites the whole base. A 0.5% threshold would therefore trade a
/// sub-millisecond worst-case query saving on a 2M-record base for roughly
/// ten times as many base rewrites. At startup, exceeding 5% also means the
/// gap since the snapshot was written is wide enough that a fresh enumeration
/// is more trustworthy than a long replay.
fn replay_exceeds_compaction_threshold(event_count: usize, base_len: usize) -> bool {
    event_count.saturating_mul(20) > base_len
}

fn resume_view(
    volume: &str,
    all_volumes: &[String],
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
    if replay_exceeds_compaction_threshold(events.len(), archived.len()) {
        return Err(anyhow::anyhow!("replay exceeds the compaction threshold"));
    }

    let mut filter = SelfWriteFilter::new(volume, all_volumes, auxiliary_marking_enabled);
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
            delta: Arc::new(delta),
            generation: scry_core::view::fresh_generation(),
            journal_id: cursor.journal_id,
            next_usn: cursor.next_usn,
            volume_serial: cursor.volume_serial,
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let host = hosting_volume(&path);
    let mark = |f: &std::fs::File| {
        if auxiliary_marking_enabled {
            let Some(host) = &host else {
                eprintln!("scryd: could not determine snapshot's hosting volume for {path:?}");
                return;
            };
            if let Err(e) = scry_fsevents::WindowsBackend::mark_handle_as_auxiliary(f, host) {
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
    let path = snapshot_path(volume);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let scratch_dir = compaction_scratch_dir(volume);
    std::fs::create_dir_all(&scratch_dir)?;
    let host = hosting_volume(&path);
    let mark = |file: &std::fs::File| {
        if auxiliary_marking_enabled {
            let Some(host) = &host else {
                eprintln!("scryd: could not determine snapshot's hosting volume for {path:?}");
                return;
            };
            if let Err(error) = scry_fsevents::WindowsBackend::mark_handle_as_auxiliary(file, host)
            {
                eprintln!("scryd: could not mark compacted index as auxiliary ({error})");
            }
        }
    };
    let mem_probe_enabled = std::env::var_os("SCRY_COMPACTION_MEM_PROBE").is_some();
    let mut on_phase = |phase: &str| {
        if mem_probe_enabled {
            eprintln!("scryd: compact[{volume}] {phase}: {:?}", process_memory());
        }
    };
    if mem_probe_enabled {
        eprintln!("scryd: compact[{volume}] start: {:?}", process_memory());
    }
    view.compact_to_snapshot(&scratch_dir, &path, &mark, &mut on_phase)?;
    Ok(Arc::new(IndexView::new(Arc::new(ArenaStore::open(&path)?))))
}

/// Scratch directory for one volume's compaction spools, next to that
/// volume's snapshot. Every file created under it is removed automatically
/// when its `Spool`/`ByteSpool` drops; the directory itself is left in place
/// between runs so repeated compactions don't pay repeated `create_dir_all`
/// races, but it holds nothing durable.
fn compaction_scratch_dir(volume: &str) -> std::path::PathBuf {
    let safe: String = volume.chars().filter(|c| c.is_alphanumeric()).collect();
    snapshot_path(volume)
        .parent()
        .expect("snapshot path always has a parent")
        .join(format!("compact-{safe}"))
}

struct BackgroundModeGuard {
    entered: bool,
}

impl BackgroundModeGuard {
    fn enter() -> Self {
        let entered = unsafe {
            ffi::SetThreadPriority(ffi::GetCurrentThread(), ffi::THREAD_MODE_BACKGROUND_BEGIN) != 0
        };
        if !entered {
            eprintln!(
                "scryd: thread background mode unavailable (win32 error {}); continuing",
                unsafe { ffi::GetLastError() }
            );
        }
        Self { entered }
    }
}

impl Drop for BackgroundModeGuard {
    fn drop(&mut self) {
        if self.entered {
            unsafe {
                ffi::SetThreadPriority(ffi::GetCurrentThread(), ffi::THREAD_MODE_BACKGROUND_END);
            }
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
    let app_data = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    app_data.join("scry").join(format!("index-{safe}.rkyv"))
}

/// The volume a snapshot file physically lives on, which is not necessarily
/// the volume it describes: snapshots are written under `%LOCALAPPDATA%`, so
/// a daemon indexing `D:` still writes that snapshot onto whatever drive the
/// user profile is on. `FSCTL_MARK_HANDLE` must be applied against the
/// hosting volume's journal, or the write is invisible to the auxiliary
/// filter and retriggers that volume's own watcher.
fn hosting_volume(path: &std::path::Path) -> Option<String> {
    use std::path::{Component, Prefix};
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Some(format!("{}:", letter as char))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The four file names (final and `.tmp`, for both the snapshot and its FRN
/// sidecar) that persisting `volume`'s index can produce, regardless of which
/// physical drive they land on.
fn snapshot_file_names(volume: &str) -> [String; 4] {
    let path = snapshot_path(volume);
    let tmp_path = path.with_extension("tmp");
    let sidecar_path = path.with_extension("frn");
    let sidecar_tmp_path = path.with_extension("frn.tmp");
    [
        path.file_name().unwrap().to_string_lossy().into_owned(),
        tmp_path.file_name().unwrap().to_string_lossy().into_owned(),
        sidecar_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        sidecar_tmp_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ]
}

/// Every filename `compact_to_snapshot` can create under `volume`'s scratch
/// directory (plus the scratch directory's own name), so its `Created`
/// events can be recognized the same way the final snapshot's can. Must be
/// kept in sync with every `dir.join(...)` in `SpooledArenaBuilder::new` and
/// `dfs::build_file_backed`/`FileBackedChildTable::build`, plus the
/// `dfs-size-prefix.spool` file `IndexView::compact_to_snapshot` creates
/// directly — a spool this list omits would retrigger a reindex of whichever
/// volume hosts the scratch directory the moment it's created.
fn compaction_scratch_names(volume: &str) -> Vec<String> {
    let dir = compaction_scratch_dir(volume);
    let mut names = vec![dir.file_name().unwrap().to_string_lossy().into_owned()];
    names.extend(
        [
            "names.spool",
            "bucket-offsets.spool",
            "parents.spool",
            "mtimes.spool",
            "sizes.spool",
            "size-exact-inputs.spool",
            "size-unknown-dfs.spool",
            "size-exact.spool",
            "trigram.spool",
            "dfs-positions.spool",
            "dfs-records.spool",
            "dfs-subtree-ends.spool",
            "dfs-visited.spool",
            "dfs-stack.spool",
            "dfs-starts.spool",
            "dfs-children.spool",
            "dfs-cursor.spool",
            "dfs-size-prefix.spool",
        ]
        .iter()
        .map(|name| (*name).to_string()),
    );
    names
}

/// Every snapshot and compaction-scratch filename that could land on
/// `watched_volume`'s journal: not just `watched_volume`'s own snapshot, but
/// any other indexed volume's snapshot and scratch files too, since they all
/// live under the same `%LOCALAPPDATA%` and so may physically share a
/// hosting drive. Without this, a daemon watching C: while also indexing D:
/// sees D:'s snapshot write (physically on C:) as an unrecognized file and
/// reindexes C: for no real change.
fn owned_snapshot_names(
    watched_volume: &str,
    all_volumes: &[String],
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for volume in all_volumes {
        let path = snapshot_path(volume);
        if hosting_volume(&path).as_deref() == Some(watched_volume) {
            names.extend(snapshot_file_names(volume));
            names.extend(compaction_scratch_names(volume));
        }
    }
    names
}

/// Holds the state `is_real_change` needs to recognize the daemon's own
/// snapshot writes: the name-based fallback set, and whether
/// `FSCTL_MARK_HANDLE` auxiliary marking is active (in which case
/// `is_auxiliary` is trusted over the heuristic).
struct SelfWriteFilter {
    own_names: std::collections::HashSet<String>,
    self_frns: std::collections::HashSet<u64>,
    use_auxiliary: bool,
}

impl SelfWriteFilter {
    fn new(watched_volume: &str, all_volumes: &[String], use_auxiliary: bool) -> Self {
        SelfWriteFilter {
            own_names: owned_snapshot_names(watched_volume, all_volumes),
            self_frns: std::collections::HashSet::new(),
            use_auxiliary,
        }
    }

    fn is_own_name(&self, name: &str) -> bool {
        self.own_names.contains(name)
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
        ChangeEvent::Modified { .. } | ChangeEvent::Advanced { .. } => false,
        ChangeEvent::Deleted { frn, .. } => {
            let was_self = state.self_frns.remove(frn);
            !was_self
        }
    }
}

fn collect_change(
    event: &scry_fsevents::ChangeEvent,
    filter: &mut SelfWriteFilter,
    batch: &mut Vec<scry_fsevents::ChangeEvent>,
    replay_next_usn: &mut Option<i64>,
) {
    if let scry_fsevents::ChangeEvent::Advanced { next_usn } = event {
        *replay_next_usn = Some(*next_usn);
    } else if is_real_change(event, filter) {
        batch.push(event.clone());
    }
}

/// Polling is cheap; persisting is not. A cursor-only advance used to rewrite
/// the full snapshot after every 30-second quiet gap, turning ordinary file
/// activity into repeated index-sized bursts. Structural changes get a much
/// shorter durability bound than cursor-only progress, while clean shutdown
/// and the 5% compaction threshold remain immediate write points.
const IDLE_PERSIST_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const STRUCTURAL_PERSIST_QUIET_PERIOD: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);
const CURSOR_CHECKPOINT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const IDLE_PERSIST_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

#[derive(Default)]
struct IdlePersistSchedule {
    dirty_since: Option<std::time::Instant>,
    last_structural_change: Option<std::time::Instant>,
    last_attempt: Option<std::time::Instant>,
}

impl IdlePersistSchedule {
    fn observe(&mut self, now: std::time::Instant, structural_change: bool, cursor_advanced: bool) {
        if structural_change || cursor_advanced {
            self.dirty_since.get_or_insert(now);
        }
        if structural_change {
            self.last_structural_change = Some(now);
        }
    }

    fn should_attempt(&self, now: std::time::Instant) -> bool {
        let due = if let Some(changed) = self.last_structural_change {
            now.duration_since(changed) >= STRUCTURAL_PERSIST_QUIET_PERIOD
        } else if let Some(dirty) = self.dirty_since {
            now.duration_since(dirty) >= CURSOR_CHECKPOINT_MAX_AGE
        } else {
            false
        };
        due && self
            .last_attempt
            .is_none_or(|attempt| now.duration_since(attempt) >= IDLE_PERSIST_RETRY_INTERVAL)
    }

    fn attempted(&mut self, now: std::time::Instant) {
        self.last_attempt = Some(now);
    }

    fn persisted(&mut self) {
        *self = Self::default();
    }
}

fn view_has_structural_delta(view: &IndexView) -> bool {
    view.delta.tombstones.count_ones() != 0 || !view.delta.added.is_empty()
}

fn view_needs_persist(view: &IndexView) -> bool {
    let archived = view.base.archived();
    view_has_structural_delta(view)
        || view.journal_id != archived.journal_id
        || view.next_usn != archived.next_usn
        || view.volume_serial != archived.volume_serial
}

fn reindex_on_changes(
    volume: String,
    all_volumes: &[String],
    rx: crossbeam::channel::Receiver<scry_fsevents::ChangeEvent>,
    store: SharedStore,
    auxiliary_marking_enabled: bool,
    watcher: &scry_fsevents::JournalHandle,
) {
    let mut filter = SelfWriteFilter::new(&volume, all_volumes, auxiliary_marking_enabled);
    let mut persist_schedule = IdlePersistSchedule::default();
    {
        let view = store.load_full();
        persist_schedule.observe(
            std::time::Instant::now(),
            view_has_structural_delta(&view),
            view_needs_persist(&view),
        );
    }

    loop {
        // Block until something changes...
        let first = match rx.recv_timeout(IDLE_PERSIST_POLL_INTERVAL) {
            Ok(event) => event,
            Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                let now = std::time::Instant::now();
                if persist_schedule.should_attempt(now) {
                    persist_schedule.attempted(now);
                    if persist_idle_view(&store, &volume, auxiliary_marking_enabled) {
                        persist_schedule.persisted();
                    }
                }
                continue;
            }
            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                eprintln!("scryd: journal watcher channel closed, live updates stopped");
                return;
            }
        };
        let mut batch = Vec::new();
        let mut replay_next_usn = None;
        collect_change(&first, &mut filter, &mut batch, &mut replay_next_usn);

        // ...then absorb a short burst of further changes before paying for
        // a full reindex, so e.g. extracting a zip doesn't trigger thousands
        // of back-to-back rebuilds.
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(500)) {
            collect_change(&ev, &mut filter, &mut batch, &mut replay_next_usn);
        }

        // The channel filled up while we were mid-reindex; a structural
        // event may have been dropped, so force a resync regardless of what
        // the drained events looked like.
        let mut needs_full_reindex = watcher.take_overflow();

        if batch.is_empty() && replay_next_usn.is_none() && !needs_full_reindex {
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
        // Every structural name has now been copied into `delta`; retaining
        // the burst alongside it through compaction pointlessly doubles the
        // live event payload at exactly the phase whose private-memory budget
        // is strictest.
        let had_structural_changes = !batch.is_empty();
        drop(batch);

        if !needs_full_reindex {
            let next = Arc::new(IndexView {
                base: view.base.clone(),
                delta: Arc::new(delta),
                generation: scry_core::view::fresh_generation(),
                journal_id: view.journal_id,
                next_usn: replay_next_usn.unwrap_or(view.next_usn),
                volume_serial: view.volume_serial,
            });
            let changes = next.delta.tombstones.count_ones() as usize + next.delta.added.len();
            if replay_exceeds_compaction_threshold(changes, next.base.archived().len()) {
                match compact_view(&next, &volume, auxiliary_marking_enabled) {
                    Ok(compacted) => {
                        let len = compacted.len();
                        store.store(compacted);
                        trim_working_set();
                        eprintln!("scryd: compacted {volume} ({len} entries)");
                        persist_schedule.persisted();
                    }
                    Err(error) => {
                        store.store(next);
                        persist_schedule.observe(
                            std::time::Instant::now(),
                            had_structural_changes,
                            replay_next_usn.is_some(),
                        );
                        eprintln!("scryd: compaction failed: {error}");
                    }
                }
            } else {
                store.store(next);
                persist_schedule.observe(
                    std::time::Instant::now(),
                    had_structural_changes,
                    replay_next_usn.is_some(),
                );
            }
            continue;
        }

        match build_view(&volume, auxiliary_marking_enabled) {
            Ok(new_view) => {
                let len = new_view.view.len();
                store.store(new_view.view);
                persist_schedule.persisted();
                trim_working_set();
                eprintln!("scryd: reindexed {volume} ({len} entries)");
            }
            Err(e) => eprintln!("scryd: reindex failed: {e}"),
        }
    }
}

/// Returns true when the view was already durable or was persisted
/// successfully. A failure leaves the caller's schedule dirty for retry.
fn persist_idle_view(store: &SharedStore, volume: &str, auxiliary_marking_enabled: bool) -> bool {
    let view = store.load_full();
    if !view_needs_persist(&view) {
        return true;
    }
    match compact_view(&view, volume, auxiliary_marking_enabled) {
        Ok(compacted) => {
            store.store(compacted);
            trim_working_set();
            eprintln!("scryd: persisted idle snapshot for {volume}");
            true
        }
        Err(error) => {
            eprintln!("scryd: idle snapshot persistence failed: {error}");
            false
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
        ChangeEvent::Modified { .. } | ChangeEvent::Advanced { .. } => None,
    }
}

/// Above this many matches, the daemon stops trusting a query's candidate
/// list as complete: the bounded top-k search inside `IndexView` only keeps
/// every match when the true count is under its limit, so a result this long
/// may have silently dropped matches that a *narrower* follow-up query would
/// need. Caching it would risk a refined query missing a file that exists —
/// the one failure mode this feature must never produce — so that volume's
/// cache entry is left empty instead, and the next keystroke rescans it.
const REFINEMENT_CACHE_CAP: usize = 20_000;

/// Worker count for `scry_core::view::search_base_parallel`'s bucket-sharded
/// scan, computed once and reused for the daemon's lifetime rather than
/// re-queried per keystroke. Capped at 8: beyond that, per-shard bucket
/// ranges get small enough that thread coordination overhead starts eating
/// into the win, and it keeps one query from claiming every core on a
/// larger machine while a reindex is also running.
fn query_thread_count() -> usize {
    static COUNT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *COUNT.get_or_init(|| {
        // Measurement-only override: lets the daemon be launched at a fixed
        // worker count (e.g. 1, for an apples-to-apples single-threaded
        // comparison) without a separate build.
        if let Some(threads) = std::env::var("SCRY_QUERY_THREADS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
        {
            return threads.max(1);
        }
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8)
    })
}

/// Measurement-only override: `SCRY_NO_REFINEMENT_CACHE=1` makes every query
/// rescan from scratch, for an as-you-type latency comparison against the
/// cached path without a separate build.
fn refinement_cache_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("SCRY_NO_REFINEMENT_CACHE").is_none())
}

/// The result of the last refinable query on this connection, per volume, so
/// the next keystroke can filter instead of rescan when it's provably a
/// narrower version of the same query. Lives for the lifetime of one
/// connection: an as-you-type session is exactly the case this optimizes,
/// and nothing about it is meaningful to persist across connections.
#[derive(Default)]
struct RefinementCache {
    kind: Option<QueryKind>,
    /// The ordering the cached candidates were collected under. A cached set
    /// is only a superset of a refined query's matches for the *same*
    /// ordering: the scan keeps the best `REFINEMENT_CACHE_CAP` by that
    /// ordering, and a different one would have kept different records.
    order: Order,
    terms: Vec<String>,
    per_volume: Vec<Option<VolumeCandidates>>,
}

struct VolumeCandidates {
    generation: u64,
    hits: Vec<Hit>,
}

/// The term list a query would need to have matched to be filterable from a
/// cached result later — `None` for kinds that are never cacheable
/// (`Regex`/`Wildcard`: a longer pattern can *grow* the match set, so no
/// subset relationship can be assumed) or `ShareIndex` (not a search at all).
fn refinable_terms(kind: QueryKind, query: &Query) -> Option<Vec<String>> {
    match (kind, query) {
        (QueryKind::Prefix, Query::Prefix(pattern))
        | (QueryKind::Substring, Query::Substring(pattern)) => Some(vec![pattern.clone()]),
        (QueryKind::PathTerms, Query::PathTerms(terms)) => Some(terms.clone()),
        _ => None,
    }
}

/// True when `new_terms` can only match a subset of what `old_terms` matched:
/// every old term, in order, is a case-insensitive prefix of the
/// correspondingly-positioned new term, and no old term is missing from the
/// new list. Extra new terms beyond `old_terms.len()` are fine — an
/// additional AND-ed term only shrinks the match set further.
fn is_refinement(old_terms: &[String], new_terms: &[String]) -> bool {
    !old_terms.is_empty()
        && new_terms.len() >= old_terms.len()
        && old_terms.iter().zip(new_terms.iter()).all(|(old, new)| {
            new.len() >= old.len()
                && new.as_bytes()[..old.len()].eq_ignore_ascii_case(old.as_bytes())
        })
}

/// Re-checks a cached hit against a refined query without touching the
/// index, mirroring the matching rules `search_base`/`search_path_terms` use
/// so a filtered cache and a full rescan never disagree.
fn matches_refined(
    view: &IndexView,
    hit: Hit,
    kind: QueryKind,
    terms_lower: &[Vec<u8>],
    name: &mut Vec<u8>,
) -> bool {
    match kind {
        QueryKind::Prefix => {
            view.name_into(hit.record, name);
            name.len() >= terms_lower[0].len()
                && name[..terms_lower[0].len()].eq_ignore_ascii_case(&terms_lower[0])
        }
        QueryKind::Substring => {
            view.name_into(hit.record, name);
            scry_core::ascii::contains_ci(name, &terms_lower[0])
        }
        QueryKind::PathTerms => view.matches_path_terms_lower(hit.record, terms_lower, name),
        QueryKind::Wildcard | QueryKind::ShareIndex | QueryKind::QueryStats => false,
    }
}

/// As `search_indexes_cancellable`, but for a refinable query kind, checks
/// whether this connection's cache holds the previous, broader query's
/// complete match set per volume and filters it in memory instead of
/// rescanning that volume. A volume whose index generation moved, or whose
/// cached set hit `REFINEMENT_CACHE_CAP` last time, is rescanned on its own —
/// one volume reindexing must not force a rescan of the others.
#[cfg(test)]
fn search_indexes_with_cache(
    indexes: &VolumeIndexes,
    kind: QueryKind,
    query: &Query,
    options: SearchOptions,
    cancel: scry_core::Cancellation,
    cache: &mut RefinementCache,
) -> Vec<ResultEntry> {
    search_indexes_with_cache_with_spans(
        indexes,
        kind,
        query,
        options,
        cancel,
        cache,
        &mut QuerySpans::default(),
    )
}

fn search_indexes_with_cache_with_spans(
    indexes: &VolumeIndexes,
    kind: QueryKind,
    query: &Query,
    options: SearchOptions,
    cancel: scry_core::Cancellation,
    cache: &mut RefinementCache,
    spans: &mut QuerySpans,
) -> Vec<ResultEntry> {
    let SearchOptions { limit, order } = options;
    if limit == 0 || cancel.is_cancelled() {
        return Vec::new();
    }
    let Some(new_terms) = refinable_terms(kind, query).filter(|_| refinement_cache_enabled())
    else {
        return search_indexes_cancellable_with_spans(indexes, query, options, cancel, spans);
    };
    let refine =
        cache.kind == Some(kind) && cache.order == order && is_refinement(&cache.terms, &new_terms);
    let terms_lower: Vec<Vec<u8>> = new_terms
        .iter()
        .map(|term| term.as_bytes().to_ascii_lowercase())
        .collect();
    if !refine || cache.per_volume.len() != indexes.len() {
        cache.per_volume.clear();
        cache.per_volume.resize_with(indexes.len(), || None);
    }

    // Overscanning to `REFINEMENT_CACHE_CAP` only pays for itself if a later
    // keystroke actually filters the result, and every hit it collects beyond
    // `limit` costs a `full_path` reconstruction (~3 µs) plus a `String`. A
    // one-shot `scry <query>` opens a connection, asks once and exits, so it
    // would pay that for nothing. Widen only once this connection has already
    // served a refinable query — that is what an as-you-type session looks
    // like, and it is the only case where the cache can be read back.
    let seen_refinable_query = cache.kind.is_some();
    let scan_limit = if seen_refinable_query {
        REFINEMENT_CACHE_CAP
    } else {
        limit
    };

    let mut views = Vec::with_capacity(indexes.len());
    let mut merged = Vec::new();
    let mut next_per_volume = Vec::with_capacity(indexes.len());
    for (i, index) in indexes.iter().enumerate() {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let view = index.store.load_full();
        let reused = refine
            .then(|| cache.per_volume[i].as_ref())
            .flatten()
            .filter(|cached| cached.generation == view.generation);
        let mut name = Vec::new();
        let hits = if let Some(cached) = reused {
            cached
                .hits
                .iter()
                .copied()
                .filter_map(|mut hit| {
                    if !matches_refined(&view, hit, kind, &terms_lower, &mut name) {
                        return None;
                    }
                    if order == Order::Relevance {
                        // Prefix/substring refinement leaves the leaf name
                        // in `name`; path matching walks ancestors, so load
                        // the leaf once here before rebuilding its rank.
                        if kind == QueryKind::PathTerms {
                            view.name_into(hit.record, &mut name);
                        }
                        let key = scry_core::rank::relevance_key(
                            hit_quality(query, &name),
                            name.len() as u32,
                            hit.record,
                        );
                        hit.rank_bits = scry_core::rank::key_rank_bits(key);
                    }
                    Some(hit)
                })
                .collect::<Vec<_>>()
        } else {
            view.search_hits_cancellable_with_spans(
                query,
                SearchOptions::ordered(scan_limit, order),
                Some(cancel),
                query_thread_count(),
                Some(&mut *spans),
            )
        };
        if cancel.is_cancelled() {
            return Vec::new();
        }
        merged.extend(hits.iter().copied().map(|hit| (i, hit)));
        // Only a set scanned at the full cap is known to be a superset of what
        // a refined query could match. A set truncated at `limit` is not, and
        // caching it would let a later keystroke miss real hits.
        next_per_volume.push(
            (scan_limit == REFINEMENT_CACHE_CAP && hits.len() < REFINEMENT_CACHE_CAP).then(|| {
                VolumeCandidates {
                    generation: view.generation,
                    hits,
                }
            }),
        );
        views.push(view);
    }
    cache.kind = Some(kind);
    cache.order = order;
    cache.terms = new_terms;
    cache.per_volume = next_per_volume;

    let merge_started = std::time::Instant::now();
    rank_sort_truncate_hits(&mut merged, limit);
    spans.merge_ns = spans
        .merge_ns
        .saturating_add(merge_started.elapsed().as_nanos() as u64);
    let started = std::time::Instant::now();
    let entries: Vec<_> = merged
        .into_iter()
        .map(|(volume, hit)| views[volume].materialize_one(&hit))
        .collect();
    spans.materialize_ns = spans
        .materialize_ns
        .saturating_add(started.elapsed().as_nanos() as u64);
    spans.emitted = spans.emitted.saturating_add(entries.len() as u64);
    entries
}

/// A connection does its read/search/write cycle on one thread — the pipe's
/// synchronous handle deadlocks if a blocking read on one thread and a
/// blocking write on another are both in flight at once (see the `Sync`
/// safety note on `scry_ipc::Pipe`) — while a second, poll-only thread bumps
/// `generation` whenever it sees unread bytes already sitting in the pipe.
/// That's the cancellation signal an in-flight search checks periodically
/// (via `Cancellation`) to abandon a stale scan once a newer request has
/// already arrived, while this thread still writes exactly one response
/// frame per request, so framing stays 1:1 even for a superseded (empty)
/// result.
fn handle_connection(pipe: scry_ipc::Pipe, indexes: &VolumeIndexes) -> std::io::Result<()> {
    let pipe = Arc::new(pipe);
    let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let peek_pipe = Arc::clone(&pipe);
    let peek_generation = Arc::clone(&generation);
    let peek_stop = Arc::clone(&stop);
    let peeker = std::thread::spawn(move || {
        while !peek_stop.load(std::sync::atomic::Ordering::Relaxed) {
            match peek_pipe.pending_bytes() {
                Ok(0) => {}
                Ok(_) => {
                    peek_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(_) => return, // client disconnected
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });

    let mut cache = RefinementCache::default();
    let result = (|| -> std::io::Result<()> {
        loop {
            let req_bytes = match pipe.read_frame() {
                Ok(b) => b,
                Err(_) => return Ok(()), // client disconnected
            };
            let Some(req) = decode_request(&req_bytes) else {
                continue;
            };
            let gen = generation.load(std::sync::atomic::Ordering::Relaxed);
            let cancel = scry_core::Cancellation::new(&generation, gen);
            let mut spans = QuerySpans::default();
            let response = handle_request(&req, indexes, &pipe, cancel, &mut cache, &mut spans)?;
            let write_started = (req.kind != QueryKind::QueryStats).then(std::time::Instant::now);
            pipe.write_frame(&response)?;
            if let Some(started) = write_started {
                spans.encode_ns = spans
                    .encode_ns
                    .saturating_add(started.elapsed().as_nanos() as u64);
                if req.kind != QueryKind::ShareIndex {
                    query_metrics().lock().unwrap().record(spans);
                }
            }
        }
    })();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = peeker.join();
    result
}

/// Builds the response frame for one request. Returns an empty results frame
/// for a search request that `cancel` reports as superseded before or during
/// the scan; `ShareIndex` requests are never cancellable (they're answered
/// from an already-published snapshot, not a scan).
fn handle_request(
    req: &scry_core::protocol::Request,
    indexes: &VolumeIndexes,
    pipe: &scry_ipc::Pipe,
    cancel: scry_core::Cancellation,
    cache: &mut RefinementCache,
    spans: &mut QuerySpans,
) -> std::io::Result<Vec<u8>> {
    // `load_full` clones the Arc, keeping this index alive for the whole
    // query even if the reindex thread swaps in a new one mid-search.
    if req.kind == QueryKind::ShareIndex && indexes.len() != 1 {
        return Ok(encode_results(&[]));
    }
    if req.kind == QueryKind::ShareIndex {
        let snapshot = indexes[0].store.load_full();
        if req.pattern.parse::<u64>().ok() == Some(snapshot.generation) {
            return Ok(encode_shared_index(&SharedIndexResponse {
                handle: 0,
                len: 0,
                generation: snapshot.generation,
                overlay: Vec::new(),
            }));
        }
        let section = shared_section(&snapshot)?;
        let handle = section.duplicate_for(pipe.client_process_id()?)?;
        return Ok(encode_shared_index(&SharedIndexResponse {
            handle,
            len: section.len() as u64,
            generation: snapshot.generation,
            overlay: snapshot.delta.encode_query_overlay(),
        }));
    }
    if req.kind == QueryKind::QueryStats {
        return Ok(query_metrics().lock().unwrap().report().into_bytes());
    }
    let query = match req.kind {
        QueryKind::Prefix => Query::Prefix(req.pattern.clone()),
        QueryKind::Substring => Query::Substring(req.pattern.clone()),
        QueryKind::Wildcard => Query::wildcard(&req.pattern),
        QueryKind::PathTerms => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .min(u32::MAX as u64) as u32;
            scry_core::terms::parse_query(&req.pattern, now)
                .unwrap_or_else(|_| Query::PathTerms(Vec::new()))
        }
        QueryKind::ShareIndex => unreachable!(),
        QueryKind::QueryStats => unreachable!(),
    };
    let entries = search_indexes_with_cache_with_spans(
        indexes,
        req.kind,
        &query,
        SearchOptions::ordered(req.limit as usize, req.order),
        cancel,
        cache,
        spans,
    );
    let started = std::time::Instant::now();
    let response = encode_results(&entries);
    spans.encode_ns = spans
        .encode_ns
        .saturating_add(started.elapsed().as_nanos() as u64);
    Ok(response)
}

/// Fans a query out across every volume's index and merges by rank, in one
/// bounded top-k pass per volume. Abandons the fan-out (returning an empty
/// result) once `cancel` reports this request was superseded.
#[cfg(test)]
fn search_indexes_cancellable(
    indexes: &VolumeIndexes,
    query: &Query,
    options: SearchOptions,
    cancel: scry_core::Cancellation,
) -> Vec<ResultEntry> {
    search_indexes_cancellable_with_spans(
        indexes,
        query,
        options,
        cancel,
        &mut QuerySpans::default(),
    )
}

fn search_indexes_cancellable_with_spans(
    indexes: &VolumeIndexes,
    query: &Query,
    options: SearchOptions,
    cancel: scry_core::Cancellation,
    spans: &mut QuerySpans,
) -> Vec<ResultEntry> {
    if options.limit == 0 || cancel.is_cancelled() {
        return Vec::new();
    }
    let mut views = Vec::with_capacity(indexes.len());
    let mut hits_by_volume = Vec::new();
    for (volume, index) in indexes.iter().enumerate() {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let view = index.store.load_full();
        let hits = view.search_hits_cancellable_with_spans(
            query,
            options,
            Some(cancel),
            query_thread_count(),
            Some(&mut *spans),
        );
        hits_by_volume.extend(hits.into_iter().map(|hit| (volume, hit)));
        views.push(view);
    }
    if cancel.is_cancelled() {
        return Vec::new();
    }
    let merge_started = std::time::Instant::now();
    rank_sort_truncate_hits(&mut hits_by_volume, options.limit);
    spans.merge_ns = spans
        .merge_ns
        .saturating_add(merge_started.elapsed().as_nanos() as u64);
    let started = std::time::Instant::now();
    let entries: Vec<_> = hits_by_volume
        .into_iter()
        .map(|(volume, hit)| views[volume].materialize_one(&hit))
        .collect();
    spans.materialize_ns = spans
        .materialize_ns
        .saturating_add(started.elapsed().as_nanos() as u64);
    spans.emitted = spans.emitted.saturating_add(entries.len() as u64);
    entries
}

/// Rank cross-volume hits before paths are reconstructed. `rank_bits` carries
/// the comparable half of the key computed during the per-volume scan; the
/// stable volume slot and then its local record finish otherwise-equal keys.
fn rank_sort_truncate_hits(hits: &mut Vec<(usize, Hit)>, limit: usize) {
    if hits.len() <= 1 {
        hits.truncate(limit);
        return;
    }
    hits.sort_unstable_by_key(|(volume, hit)| (hit.rank_bits, *volume, hit.record));
    hits.truncate(limit);
}

fn hit_quality(query: &Query, name: &[u8]) -> u8 {
    match query {
        Query::Prefix(pattern) | Query::Substring(pattern) => {
            if name.eq_ignore_ascii_case(pattern.as_bytes()) {
                0
            } else if name.len() >= pattern.len()
                && name[..pattern.len()].eq_ignore_ascii_case(pattern.as_bytes())
            {
                1
            } else {
                2
            }
        }
        Query::PathTerms(terms) | Query::FilteredPathTerms { terms, .. } => terms
            .iter()
            .filter(|term| !scry_core::ascii::contains_ci(name, term.as_bytes()))
            .count()
            as u8,
        Query::Regex(_) => 2,
    }
}

/// Cache key for [`shared_section`]. Keying on `Arc::as_ptr(&view.base)` is an
/// ABA hazard: once a generation's base is dropped, the allocator can reuse
/// its address for a later generation, and the cache would then hand out a
/// stale mapping under a key that looks fresh. `generation` is monotonic
/// (`scry_core::view::fresh_generation`) and never reused, so it cannot
/// collide this way.
fn shared_section_cache_key(view: &IndexView) -> u64 {
    view.generation
}

/// Bound on the number of generations the section cache keeps entries for.
/// A client mid-request may still hold a reference to the previous
/// generation's shared section, so evicting immediately on publish would
/// race a concurrent reader; keeping 2 covers "one in flight, one current"
/// without letting the cache grow forever across a long-running daemon's
/// lifetime of compactions.
const SHARED_SECTION_CACHE_GENERATIONS: usize = 2;

// Each entry holds `base` alongside the section so the section can never
// outlive, or be mismatched with, the memory it maps.
type SectionCache = std::sync::Mutex<Vec<(u64, Arc<ArenaStore>, Arc<scry_ipc::Section>)>>;

fn shared_section_from_cache(
    view: &IndexView,
    cache: &SectionCache,
) -> std::io::Result<Arc<scry_ipc::Section>> {
    let key = shared_section_cache_key(view);
    let mut cache = cache.lock().unwrap();
    if let Some((_, _, section)) = cache.iter().find(|(cached_key, ..)| *cached_key == key) {
        return Ok(section.clone());
    }
    let section = Arc::new(scry_ipc::Section::from_file(view.base.snapshot_file())?);
    cache.push((key, view.base.clone(), section.clone()));
    if cache.len() > SHARED_SECTION_CACHE_GENERATIONS {
        cache.remove(0);
    }
    Ok(section)
}

fn shared_section(view: &IndexView) -> std::io::Result<Arc<scry_ipc::Section>> {
    static CACHE: std::sync::OnceLock<SectionCache> = std::sync::OnceLock::new();
    shared_section_from_cache(
        view,
        CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new())),
    )
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

    #[test]
    fn local_and_rpc_results_agree() {
        let dir = tempfile::tempdir().unwrap();
        let view = build_store_with_n_records(200, &dir);
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

        // Same query, both paths, over the same live connection: the RPC
        // round trip and the in-process local mapping must agree exactly.
        let rpc = client.query(QueryKind::Substring, "file", 50).unwrap();
        let local = client
            .search_local(QueryKind::Substring, "file", 50)
            .unwrap();
        assert_eq!(rpc, local);

        drop(client);
        thread.join().unwrap();
    }

    #[test]
    fn shared_section_key_is_unique_per_generation() {
        // Two views sharing the exact same `base` allocation (as happens right
        // before the allocator would reuse a freed generation's address) must
        // still get distinct cache keys, or `shared_section` hands out a stale
        // mapping under a key that looks fresh (the ABA hazard this guards).
        let dir = tempfile::tempdir().unwrap();
        let view1 = build_store_with_n_records(10, &dir);
        let view2 = IndexView {
            base: view1.base.clone(),
            delta: view1.delta.clone(),
            generation: view1.generation + 1,
            journal_id: view1.journal_id,
            next_usn: view1.next_usn,
            volume_serial: view1.volume_serial,
        };
        assert_ne!(
            shared_section_cache_key(&view1),
            shared_section_cache_key(&view2),
            "same base allocation, different generation: keys must not collide"
        );
    }

    #[test]
    fn shared_section_cache_keeps_base_alive() {
        let dir = tempfile::tempdir().unwrap();
        let cache: SectionCache = std::sync::Mutex::new(Vec::new());
        let view = build_store_with_n_records(10, &dir);
        let base_weak = Arc::downgrade(&view.base);
        shared_section_from_cache(&view, &cache).unwrap();
        // Drop every strong reference this test holds directly; the cache
        // entry inserted by `shared_section_from_cache` must be the one
        // keeping it alive.
        drop(view);
        assert!(
            base_weak.upgrade().is_some(),
            "cached section entry did not keep its base alive"
        );
    }

    #[test]
    fn shared_section_cache_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let cache: SectionCache = std::sync::Mutex::new(Vec::new());
        let stale = build_store_with_n_records(10, &dir);
        let stale_weak = Arc::downgrade(&stale.base);
        shared_section_from_cache(&stale, &cache).unwrap();
        drop(stale);
        for n in 11..11 + SHARED_SECTION_CACHE_GENERATIONS {
            let view = build_store_with_n_records(n, &dir);
            shared_section_from_cache(&view, &cache).unwrap();
        }
        assert!(
            stale_weak.upgrade().is_none(),
            "cache grew past its documented bound of {SHARED_SECTION_CACHE_GENERATIONS} generations"
        );
        assert!(cache.lock().unwrap().len() <= SHARED_SECTION_CACHE_GENERATIONS);
    }

    #[test]
    fn cancelled_request_returns_empty_results() {
        let dir = tempfile::tempdir().unwrap();
        let view = build_store_with_n_records(200, &dir);
        let indexes = Arc::new(vec![VolumeIndex {
            volume: "C:".to_string(),
            store: Arc::new(arc_swap::ArcSwap::from(view)),
            cursor: None,
        }]);
        let generation = std::sync::atomic::AtomicU64::new(1);
        let cancel = scry_core::Cancellation::new(&generation, 0); // stale: expects 0, generation is 1
        let entries = search_indexes_cancellable(
            &indexes,
            &Query::Substring("file".into()),
            SearchOptions::new(50),
            cancel,
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn two_pipelined_requests_produce_exactly_two_response_frames() {
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
        let client = (0..100)
            .find_map(|_| {
                scry_ipc::connect_client(&pipe_name).ok().or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    None
                })
            })
            .expect("test pipe did not become ready");

        let request = scry_core::protocol::Request {
            kind: QueryKind::Substring,
            pattern: "file".to_string(),
            limit: 50,
            order: Order::default(),
        };
        // Both requests are written before either response is read, so the
        // daemon's peek thread should see the second one arrive while (or
        // before) the first is still being handled — but whether it actually
        // supersedes the first is a timing race, not something this test can
        // assert on. What must always hold is the framing invariant: exactly
        // one response frame per request, in request order.
        client
            .write_frame(&scry_core::protocol::encode_request(&request))
            .unwrap();
        client
            .write_frame(&scry_core::protocol::encode_request(&request))
            .unwrap();

        let first = scry_core::protocol::decode_results(&client.read_frame().unwrap()).unwrap();
        let second = scry_core::protocol::decode_results(&client.read_frame().unwrap()).unwrap();
        assert!(first.is_empty() || first == expected);
        assert_eq!(second, expected);

        drop(client);
        thread.join().unwrap();
    }

    fn build_refinement_store(dir: &tempfile::TempDir) -> Arc<IndexView> {
        let mut b = Arena::builder();
        let root = b.push("C:", 0, true);
        let dirs = ["Documents", "Photos", "Projects"];
        let vocab = [
            "ledger", "report", "invoice", "photo", "backup", "notes", "draft", "project",
        ];
        let exts = ["txt", "pdf", "png", "docx"];
        let mut i: u32 = 0;
        for &d in &dirs {
            let dnode = b.push(d, 0, true);
            b.set_parent(dnode, root);
            for &w in &vocab {
                for &ext in &exts {
                    let name = format!("{w}_{i}.{ext}");
                    let f = b.push(&name, 0, false);
                    b.set_parent(f, dnode);
                    i += 1;
                }
            }
        }
        let arena = b.build().0;
        let path = dir.path().join("refine.rkyv");
        save(&arena, &path).unwrap();
        Arc::new(IndexView::new(Arc::new(ArenaStore::open(&path).unwrap())))
    }

    fn single_volume(view: Arc<IndexView>) -> VolumeIndexes {
        Arc::new(vec![VolumeIndex {
            volume: "C:".to_string(),
            store: Arc::new(arc_swap::ArcSwap::from(view)),
            cursor: None,
        }])
    }

    /// `contains_ci` only lowercases the haystack, not the needle, so the
    /// per-request normalization passed to the refinement filter must handle
    /// uppercase terms before any cached hit is visited.
    #[test]
    fn matches_refined_substring_is_case_insensitive_regardless_of_which_side_carries_the_case() {
        let dir = tempfile::tempdir().unwrap();
        let view = build_refinement_store(&dir);
        let hit = view.search_hits(&Query::Substring("ledger".into()), SearchOptions::new(1))[0];
        let mut name = Vec::new();
        assert!(matches_refined(
            &view,
            hit,
            QueryKind::Substring,
            &[b"ledger".to_vec()],
            &mut name,
        ));
    }

    /// Filtering a cached candidate set must always agree with rescanning the
    /// index from scratch, at every keystroke of a randomised typing
    /// sequence. A disagreement here would mean a refined query could hide a
    /// file that a full scan would have found.
    #[test]
    fn refinement_matches_a_full_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let view = build_refinement_store(&dir);
        let indexes = single_volume(view);
        let generation = std::sync::atomic::AtomicU64::new(0);
        let vocab = [
            "ledger", "report", "invoice", "photo", "backup", "notes", "draft", "project",
        ];
        let mut rng: u64 = 0x243F_6A88_85A3_08D3;
        let mut next_rand = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        for _ in 0..1000 {
            let mut cache = RefinementCache::default();
            let word = vocab[(next_rand() as usize) % vocab.len()];
            let len = 1 + (next_rand() as usize) % word.len();
            for step in 1..=len {
                let pattern = word[..step].to_string();
                let cached = search_indexes_with_cache(
                    &indexes,
                    QueryKind::Substring,
                    &Query::Substring(pattern.clone()),
                    SearchOptions::new(50),
                    scry_core::Cancellation::new(&generation, 0),
                    &mut cache,
                );
                // A fresh cache never reuses a prior candidate list, so this
                // always takes the full-rescan branch — the baseline the
                // possibly-refined `cached` result above must match exactly.
                let mut uncached = RefinementCache::default();
                let fresh = search_indexes_with_cache(
                    &indexes,
                    QueryKind::Substring,
                    &Query::Substring(pattern.clone()),
                    SearchOptions::new(50),
                    scry_core::Cancellation::new(&generation, 0),
                    &mut uncached,
                );
                assert_eq!(cached, fresh, "diverged at pattern {pattern:?}");
            }
        }
    }

    /// A `Wildcard` query can only grow the match set as characters are
    /// added, so it must never be answered by filtering a narrower cached
    /// `Substring`/`Prefix`/`PathTerms` set — and must not overwrite that
    /// cache either, since it isn't itself refinable.
    #[test]
    fn refinement_is_bypassed_for_regex() {
        let dir = tempfile::tempdir().unwrap();
        let view = build_refinement_store(&dir);
        let indexes = single_volume(view);
        let generation = std::sync::atomic::AtomicU64::new(0);
        let mut cache = RefinementCache::default();

        search_indexes_with_cache(
            &indexes,
            QueryKind::Substring,
            &Query::Substring("resu".to_string()),
            SearchOptions::new(50),
            scry_core::Cancellation::new(&generation, 0),
            &mut cache,
        );
        assert_eq!(cache.kind, Some(QueryKind::Substring));

        let wildcard_query = Query::wildcard("*.pdf");
        let actual = search_indexes_with_cache(
            &indexes,
            QueryKind::Wildcard,
            &wildcard_query,
            SearchOptions::new(50),
            scry_core::Cancellation::new(&generation, 0),
            &mut cache,
        );
        let expected = search_indexes_cancellable(
            &indexes,
            &wildcard_query,
            SearchOptions::new(50),
            scry_core::Cancellation::new(&generation, 0),
        );
        assert_eq!(actual, expected);
        assert_eq!(cache.kind, Some(QueryKind::Substring));
    }

    /// A one-shot `scry <query>` asks its connection exactly one question and
    /// exits, so overscanning to `REFINEMENT_CACHE_CAP` on that first query
    /// buys nothing and costs a `full_path` reconstruction per surplus hit.
    /// The wide scan must therefore start only once a connection has shown it
    /// is an as-you-type session by asking a second refinable question.
    #[test]
    fn first_query_on_a_connection_does_not_overscan_for_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let view = build_refinement_store(&dir);
        let indexes = single_volume(view);
        let generation = std::sync::atomic::AtomicU64::new(0);
        let mut cache = RefinementCache::default();

        search_indexes_with_cache(
            &indexes,
            QueryKind::Substring,
            &Query::Substring("re".to_string()),
            SearchOptions::new(5),
            scry_core::Cancellation::new(&generation, 0),
            &mut cache,
        );
        assert!(
            cache.per_volume.iter().all(Option::is_none),
            "a set truncated at the caller's limit is not a superset of what a \
             refined query could match, so it must not be cached"
        );

        search_indexes_with_cache(
            &indexes,
            QueryKind::Substring,
            &Query::Substring("res".to_string()),
            SearchOptions::new(5),
            scry_core::Cancellation::new(&generation, 0),
            &mut cache,
        );
        assert!(
            cache.per_volume.iter().all(Option::is_some),
            "the second refinable query on a connection should scan wide and cache"
        );
    }

    /// Before caching `Hit`s instead of `ResultEntry`s, a refined query paid a
    /// `full_path` reconstruction for every one of the up to
    /// `REFINEMENT_CACHE_CAP` overscanned candidates instead of just the
    /// emitted `limit`. `materialize_one` is called exactly once per emitted
    /// entry, so `spans.emitted` is a direct proxy for that call count.
    #[test]
    fn refined_query_materializes_at_most_limit_entries() {
        let dir = tempfile::tempdir().unwrap();
        // Larger than `REFINEMENT_CACHE_CAP` so the second query's cache
        // population actually exercises the wide overscan this test guards.
        let view = build_store_with_n_records(30_000, &dir);
        let indexes = single_volume(view);
        let generation = std::sync::atomic::AtomicU64::new(0);
        let mut cache = RefinementCache::default();
        let limit = 50;

        // First refinable query on the connection: establishes the cache but
        // does not itself overscan (see the test above).
        search_indexes_with_cache_with_spans(
            &indexes,
            QueryKind::Substring,
            &Query::Substring("f".to_string()),
            SearchOptions::new(limit),
            scry_core::Cancellation::new(&generation, 0),
            &mut cache,
            &mut QuerySpans::default(),
        );

        let mut spans = QuerySpans::default();
        let results = search_indexes_with_cache_with_spans(
            &indexes,
            QueryKind::Substring,
            &Query::Substring("fi".to_string()),
            SearchOptions::new(limit),
            scry_core::Cancellation::new(&generation, 0),
            &mut cache,
            &mut spans,
        );
        assert!(results.len() <= limit);
        assert!(
            spans.emitted as usize <= limit,
            "materialized {} entries for a limit of {limit}",
            spans.emitted
        );
    }

    /// Attributable plan-017 wall-time probe. The first keystroke primes the
    /// per-connection state; only the second, 20,000-candidate overscan is
    /// timed. The same function can be applied to the pre-change parent for a
    /// direct before/after comparison.
    #[test]
    #[ignore = "release-mode bounded-refinement benchmark"]
    fn benchmark_bounded_refinement_wall() {
        const ITERATIONS: usize = 60;
        let dir = tempfile::tempdir().unwrap();
        let view = build_store_with_n_records(30_000, &dir);
        let indexes = single_volume(view);
        let generation = std::sync::atomic::AtomicU64::new(0);
        let mut samples = Vec::with_capacity(ITERATIONS);
        let mut merge_samples = Vec::with_capacity(ITERATIONS);

        for _ in 0..ITERATIONS {
            let mut cache = RefinementCache::default();
            let _ = search_indexes_with_cache(
                &indexes,
                QueryKind::Substring,
                &Query::Substring("f".to_string()),
                SearchOptions::new(50),
                scry_core::Cancellation::new(&generation, 0),
                &mut cache,
            );
            let started = std::time::Instant::now();
            let mut spans = QuerySpans::default();
            let results = search_indexes_with_cache_with_spans(
                &indexes,
                QueryKind::Substring,
                &Query::Substring("fi".to_string()),
                SearchOptions::new(50),
                scry_core::Cancellation::new(&generation, 0),
                &mut cache,
                &mut spans,
            );
            samples.push(started.elapsed());
            merge_samples.push(std::time::Duration::from_nanos(spans.merge_ns));
            assert_eq!(results.len(), 50);
        }

        samples.sort_unstable();
        merge_samples.sort_unstable();
        println!(
            "bounded refinement: n={} p50={:?} p99={:?}; merge p50={:?} p99={:?}",
            samples.len(),
            samples[samples.len() / 2],
            samples[samples.len() * 99 / 100],
            merge_samples[merge_samples.len() / 2],
            merge_samples[merge_samples.len() * 99 / 100],
        );
    }

    /// Not a pass/fail test: exercises the daemon's real query path (cache,
    /// span accumulation, memory sampling) end to end over a synthetic
    /// corpus and prints a report, so the numbers in
    /// `docs/query-latency-baseline.md` come from this code path rather than
    /// from a description of it. Run with
    /// `cargo test -p scry-daemon --release -- --ignored span_report --nocapture`.
    #[test]
    #[ignore = "prints a report; not a pass/fail gate"]
    fn span_report_over_a_synthetic_corpus() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = Arena::builder();
        let root = b.push("C:", 0, true);
        let teamdir = b.push("TEAMDIR", 0, true);
        b.set_parent(teamdir, root);
        let dirs = ["Documents", "Photos", "Projects", "Downloads"];
        let vocab = [
            "ledger", "report", "invoice", "photo", "backup", "notes", "draft", "project",
            "budget", "sketch", "summary", "ticket",
        ];
        let exts = ["txt", "pdf", "png", "docx", "log", "dll"];
        let mut i: u32 = 0;
        for &parent in &[teamdir, root] {
            for &d in &dirs {
                let dnode = b.push(&format!("{d}_{parent}"), 0, true);
                b.set_parent(dnode, parent);
                for &w in &vocab {
                    for &ext in &exts {
                        for _ in 0..12 {
                            let name = format!("{w}_{i}.{ext}");
                            let f = b.push(&name, 1_700_000_000, false);
                            b.set_parent(f, dnode);
                            i += 1;
                        }
                    }
                }
            }
        }
        let arena = b.build().0;
        let path = dir.path().join("report.rkyv");
        save(&arena, &path).unwrap();
        let view = Arc::new(IndexView::new(Arc::new(ArenaStore::open(&path).unwrap())));
        println!("\ncorpus: {i} records");
        let indexes = single_volume(view);
        let generation = std::sync::atomic::AtomicU64::new(0);

        let report_one = |label: &str, cache: &mut RefinementCache, kind, query: &Query| {
            let mut spans = QuerySpans::default();
            let entries = search_indexes_with_cache_with_spans(
                &indexes,
                kind,
                query,
                SearchOptions::new(50),
                scry_core::Cancellation::new(&generation, 0),
                cache,
                &mut spans,
            );
            println!(
                "  {label:22} hits={:<6} select_ns={:<9} finalize_ns={:<8} merge_ns={:<8} materialize_ns={:<8} \
                 candidates={:<7} emitted={}",
                entries.len(),
                spans.select_ns,
                spans.finalize_ns,
                spans.merge_ns,
                spans.materialize_ns,
                spans.candidates,
                spans.emitted,
            );
        };

        println!("\nmemory, cold:");
        println!("  {:?}", process_memory());

        println!("\nkeystroke sequence \"ledger\" (first query, then each refinement):");
        let mut cache = RefinementCache::default();
        let word = "ledger";
        for step in 1..=word.len() {
            let pattern = word[..step].to_string();
            let label = if step == 1 {
                "first (\"l\")".to_string()
            } else {
                pattern.clone()
            };
            report_one(
                &label,
                &mut cache,
                QueryKind::Substring,
                &Query::Substring(pattern),
            );
        }

        println!("\ncold query \".pdf\" (fresh connection):");
        let mut cache = RefinementCache::default();
        report_one(
            ".pdf",
            &mut cache,
            QueryKind::Substring,
            &Query::Substring("pdf".to_string()),
        );

        println!("\ncold query \"TEAMDIR ledger\" (PathTerms, fresh connection):");
        let mut cache = RefinementCache::default();
        report_one(
            "TEAMDIR ledger",
            &mut cache,
            QueryKind::PathTerms,
            &Query::PathTerms(vec!["TEAMDIR".to_string(), "ledger".to_string()]),
        );

        println!("\nmemory, warm (after the above):");
        println!("  {:?}", process_memory());
    }

    /// The merge across volumes has only `ResultEntry`s to sort, so a bug
    /// here would show up as results that are correctly ranked *within* each
    /// volume and shuffled across them — which no per-volume test can catch.
    #[cfg(any())]
    #[test]
    fn the_cross_volume_merge_honors_the_requested_order() {
        let entry = |path: &str, size: u64, mtime: u32| ResultEntry {
            path: path.to_string(),
            size,
            mtime,
            is_dir: false,
            size_exact: false,
        };
        let query = Query::Substring("report".to_string());
        let merged = vec![
            entry(
                r"C:
eport.txt",
                10,
                300,
            ),
            entry(
                r"D:
eport.txt",
                30,
                100,
            ),
            entry(
                r"C:
eport.txt",
                20,
                200,
            ),
        ];

        let paths = |ordering| {
            let mut entries = merged.clone();
            rank_sort_truncate(&query, ordering, &mut entries, 10);
            entries
                .into_iter()
                .map(|entry| entry.path)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            paths(Order::Recent),
            [
                r"C:
eport.txt",
                r"C:
eport.txt",
                r"D:
eport.txt"
            ]
        );
        assert_eq!(
            paths(Order::Largest),
            [
                r"D:
eport.txt",
                r"C:
eport.txt",
                r"C:
eport.txt"
            ]
        );

        let mut truncated = merged.clone();
        rank_sort_truncate(&query, Order::Largest, &mut truncated, 1);
        assert_eq!(truncated.len(), 1);
        assert_eq!(
            truncated[0].path,
            r"D:
eport.txt"
        );
    }

    /// A bug here would leave results correctly ranked within each volume but
    /// shuffled across them. Rank bits compare first; volume and its local
    /// record are deterministic tie-breakers only.
    #[test]
    fn the_cross_volume_merge_honors_carried_rank_bits() {
        let hit = |record, rank_bits| Hit {
            record,
            rank_bits,
            size: 0,
            mtime: 0,
            is_dir: false,
            size_exact: false,
        };
        let mut hits = vec![
            (1, hit(2, 30)),
            (0, hit(9, 10)),
            (1, hit(7, 10)),
            (0, hit(3, 20)),
        ];
        rank_sort_truncate_hits(&mut hits, 3);
        assert_eq!(
            hits.iter()
                .map(|(volume, hit)| (*volume, hit.record))
                .collect::<Vec<_>>(),
            [(0, 9), (1, 7), (0, 3)]
        );
    }

    #[test]
    fn cross_volume_merge_matches_a_union_sort_at_every_limit() {
        let hit = |record, rank_bits| Hit {
            record,
            rank_bits,
            size: 0,
            mtime: 0,
            is_dir: false,
            size_exact: false,
        };
        let input = vec![
            (2, hit(8, 4)),
            (0, hit(9, 1)),
            (1, hit(2, 4)),
            (0, hit(3, 4)),
            (1, hit(7, 2)),
        ];
        let mut expected = input.clone();
        expected.sort_unstable_by_key(|(volume, hit)| (hit.rank_bits, *volume, hit.record));

        for limit in 0..=input.len() + 1 {
            let mut actual = input.clone();
            rank_sort_truncate_hits(&mut actual, limit);
            let mut want = expected.clone();
            want.truncate(limit);
            assert_eq!(actual, want, "limit {limit}");
        }
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
        let mut filter = SelfWriteFilter::new("C:", &["C:".to_string()], false);

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
        let mut aux_filter = SelfWriteFilter::new("C:", &["C:".to_string()], true);
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

    #[test]
    fn journal_advance_is_retained_without_creating_a_delta_event() {
        let mut filter = SelfWriteFilter::new("C:", &["C:".to_string()], false);
        let mut batch = Vec::new();
        let mut next_usn = None;
        collect_change(
            &ChangeEvent::Advanced { next_usn: 42 },
            &mut filter,
            &mut batch,
            &mut next_usn,
        );
        assert!(batch.is_empty());
        assert_eq!(next_usn, Some(42));
    }

    #[test]
    fn cursor_only_progress_waits_for_the_hourly_checkpoint() {
        let start = std::time::Instant::now();
        let mut schedule = IdlePersistSchedule::default();
        schedule.observe(start, false, true);
        // Later cursor advances must not slide the deadline forever on a
        // continuously active volume; this is a maximum checkpoint age.
        schedule.observe(
            start + CURSOR_CHECKPOINT_MAX_AGE - std::time::Duration::from_secs(1),
            false,
            true,
        );

        assert!(!schedule
            .should_attempt(start + CURSOR_CHECKPOINT_MAX_AGE - std::time::Duration::from_secs(1)));
        assert!(schedule.should_attempt(start + CURSOR_CHECKPOINT_MAX_AGE));
    }

    #[test]
    fn structural_progress_waits_for_a_real_quiet_period() {
        let start = std::time::Instant::now();
        let mut schedule = IdlePersistSchedule::default();
        schedule.observe(start, true, true);
        schedule.observe(start + std::time::Duration::from_secs(9 * 60), true, true);

        assert!(!schedule.should_attempt(
            start + std::time::Duration::from_secs(19 * 60) - std::time::Duration::from_secs(1)
        ));
        assert!(schedule.should_attempt(start + std::time::Duration::from_secs(19 * 60)));
    }

    #[test]
    fn failed_idle_persist_retries_with_backoff_and_success_clears_it() {
        let start = std::time::Instant::now();
        let due = start + STRUCTURAL_PERSIST_QUIET_PERIOD;
        let mut schedule = IdlePersistSchedule::default();
        schedule.observe(start, true, true);
        assert!(schedule.should_attempt(due));

        schedule.attempted(due);
        assert!(!schedule
            .should_attempt(due + IDLE_PERSIST_RETRY_INTERVAL - std::time::Duration::from_secs(1)));
        assert!(schedule.should_attempt(due + IDLE_PERSIST_RETRY_INTERVAL));

        schedule.persisted();
        assert!(!schedule.should_attempt(due + IDLE_PERSIST_RETRY_INTERVAL));
    }

    #[test]
    fn snapshots_use_the_per_user_data_directory() {
        let path = snapshot_path("C:");
        assert_eq!(path.file_name().unwrap(), "index-C.rkyv");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "scry");
    }

    #[test]
    fn a_sibling_volumes_snapshot_write_never_triggers_this_volumes_reindex() {
        // Every volume's snapshot lands under the same %LOCALAPPDATA%, so two
        // indexed volumes' snapshots always share one hosting drive — the
        // drive backing the user profile, whatever it is on this machine.
        // The daemon watching that hosting volume must recognize the other
        // volume's snapshot filenames as its own writes, not just its own.
        let host = hosting_volume(&snapshot_path("D:")).expect("snapshot path has a drive letter");
        let all_volumes = vec![host.clone(), "D:".to_string()];
        let mut filter = SelfWriteFilter::new(&host, &all_volumes, false);

        for name in snapshot_file_names("D:") {
            let created = ChangeEvent::Created {
                frn: 10,
                parent_frn: 0,
                name: name.clone(),
                is_dir: false,
                is_auxiliary: false,
            };
            assert!(
                !is_real_change(&created, &mut filter),
                "{name} is D:'s own snapshot file, physically hosted on {host}; \
                 {host}'s watcher must not treat writing it as a real change"
            );

            let renamed = ChangeEvent::Renamed {
                frn: 10,
                parent_frn: 0,
                name,
                is_auxiliary: false,
            };
            assert!(!is_real_change(&renamed, &mut filter));
        }
    }

    #[test]
    fn a_sibling_volumes_compaction_scratch_write_never_triggers_this_volumes_reindex() {
        // Compaction's spool files land in the same scratch directory
        // structure as the snapshot itself, on the same hosting drive, so
        // they need the identical name-based fallback treatment.
        let host = hosting_volume(&snapshot_path("D:")).expect("snapshot path has a drive letter");
        let all_volumes = vec![host.clone(), "D:".to_string()];
        let mut filter = SelfWriteFilter::new(&host, &all_volumes, false);

        for name in compaction_scratch_names("D:") {
            let created = ChangeEvent::Created {
                frn: 10,
                parent_frn: 0,
                name: name.clone(),
                is_dir: false,
                is_auxiliary: false,
            };
            assert!(
                !is_real_change(&created, &mut filter),
                "{name} is D:'s own compaction scratch file, physically hosted on {host}; \
                 {host}'s watcher must not treat writing it as a real change"
            );
        }
    }

    #[test]
    fn replay_threshold_matches_the_compaction_threshold() {
        assert!(!replay_exceeds_compaction_threshold(5, 100));
        assert!(replay_exceeds_compaction_threshold(6, 100));
    }

    /// The console handler exits the process, so only the classification is
    /// tested directly. `CTRL_C`/`BREAK`/`CLOSE`/`LOGOFF`/`SHUTDOWN` are the
    /// signals that mean the process is going away; anything else (Windows
    /// reserves the range for future use) must not trigger a shutdown write.
    #[test]
    fn only_termination_signals_are_treated_as_shutdown() {
        assert!(is_shutdown_signal(ffi::CTRL_C_EVENT));
        assert!(is_shutdown_signal(ffi::CTRL_BREAK_EVENT));
        assert!(is_shutdown_signal(ffi::CTRL_CLOSE_EVENT));
        assert!(is_shutdown_signal(ffi::CTRL_LOGOFF_EVENT));
        assert!(is_shutdown_signal(ffi::CTRL_SHUTDOWN_EVENT));
        assert!(!is_shutdown_signal(3));
        assert!(!is_shutdown_signal(4));
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

    /// Real-volume diagnostic for Part E's STOP gate. Run elevated with
    /// `cargo test -p scry-daemon --release -- --ignored --nocapture
    /// query_p99_during_full_index_with_and_without_background_mode`.
    /// Generic vocabulary keeps the output safe to retain in measurement
    /// notes. This is ignored because it enumerates C: twice.
    #[test]
    #[ignore = "elevated real-volume contention benchmark"]
    fn query_p99_during_full_index_with_and_without_background_mode() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        struct Sample {
            p50: std::time::Duration,
            p99: std::time::Duration,
            queries: usize,
            reindex_wall: std::time::Duration,
            records: usize,
        }

        fn run(view: &Arc<IndexView>, background: bool) -> Sample {
            trim_working_set();
            let query = Query::Substring("a".to_string());
            // Give both halves the same warm mapped-index starting point, so
            // the second run cannot win merely because the first faulted the
            // query columns into the system cache.
            for _ in 0..3 {
                let _ = view.search_hits(&query, SearchOptions::new(50));
            }
            let started = Arc::new(std::sync::Barrier::new(2));
            let done = Arc::new(AtomicBool::new(false));
            let worker_started = started.clone();
            let worker_done = done.clone();
            let worker = std::thread::spawn(move || {
                let _background = background.then(BackgroundModeGuard::enter);
                worker_started.wait();
                let began = std::time::Instant::now();
                let (arena, _) = scry_fsevents::WindowsBackend::bulk_index_volume("C:")
                    .expect("elevated C: enumeration failed");
                let result = (began.elapsed(), arena.len());
                drop(arena);
                worker_done.store(true, AtomicOrdering::Release);
                result
            });

            started.wait();
            let mut latencies = Vec::new();
            while !done.load(AtomicOrdering::Acquire) {
                let began = std::time::Instant::now();
                let _ = view.search_hits(&query, SearchOptions::new(50));
                latencies.push(began.elapsed());
            }
            let (reindex_wall, records) = worker.join().unwrap();
            assert!(
                latencies.len() >= 20,
                "reindex ended before a useful sample"
            );
            latencies.sort_unstable();
            Sample {
                p50: latencies[latencies.len() / 2],
                p99: latencies[latencies.len() * 99 / 100],
                queries: latencies.len(),
                reindex_wall,
                records,
            }
        }

        scry_fsevents::configure_index_read_cap(128 * 1024 * 1024);
        let view = Arc::new(IndexView::new(Arc::new(
            ArenaStore::open(&snapshot_path("C:")).expect("C: snapshot is required"),
        )));
        let normal = run(&view, false);
        let background = run(&view, true);
        println!(
            "foreground-priority reindex: p50={:?} p99={:?} queries={} wall={:?} records={}",
            normal.p50, normal.p99, normal.queries, normal.reindex_wall, normal.records
        );
        println!(
            "background-priority reindex: p50={:?} p99={:?} queries={} wall={:?} records={}",
            background.p50,
            background.p99,
            background.queries,
            background.reindex_wall,
            background.records
        );
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
