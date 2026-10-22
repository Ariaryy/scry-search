#![cfg_attr(windows, windows_subsystem = "windows")]

//! scryd: background indexing daemon. Bulk-indexes an NTFS volume via
//! scry-fsevents, serves queries over a named pipe, and keeps the index
//! current by watching the USN journal.
//!
//! Each volume publishes its immutable base and live delta together. Journal
//! updates patch the delta; compaction streams a new snapshot to disk. This
//! entry point wires startup, background workers, and the named-pipe server.

mod changes;
mod ffi;
mod metrics;
mod options;
mod paths;
mod persistence;
mod platform;
mod rpc;
mod search;
mod sections;
mod shutdown;
mod updates;

use options::startup_options;
use persistence::build_or_resume_view;
use platform::{
    configure_background_qos, configure_background_thread_qos, configure_index_read_cap,
};
use rpc::serve_indexes;
use updates::{persist_idle_view, reindex_on_changes, spawn_volume_watcher};

/// The Windows default heap does not return large freed spans to the OS
/// promptly, which is why a reindex spike leaves RSS elevated long after the
/// allocation is gone. mimalloc purges on a timer instead.
#[cfg(windows)]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use scry_core::{ArenaStore, IndexView};
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

static BENCHMARK_FROZEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static BENCHMARK_CACHE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

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

    // Read-only experiment: use the production RPC/refinement/section paths,
    // but never replay, watch, compact, persist, or open a raw volume. This
    // needs no elevation and cannot take the installed daemon's pipe name.
    if !options.benchmark_snapshot.is_empty() {
        BENCHMARK_FROZEN.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut indexes = Vec::new();
        for snapshot in options.benchmark_snapshot {
            let base = Arc::new(ArenaStore::open(&snapshot.path)?);
            let mut view = IndexView::new(base);
            if let Some(path) = snapshot.overlay {
                use std::io::Read;
                let mut bytes = Vec::new();
                std::fs::File::open(path)?
                    .take(64 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
                anyhow::ensure!(
                    bytes.len() <= 64 * 1024 * 1024,
                    "benchmark overlay exceeds 64 MiB"
                );
                view.delta = Arc::new(
                    scry_core::delta::Delta::decode_query_overlay(
                        &bytes,
                        view.base.archived().len(),
                    )
                    .ok_or_else(|| anyhow::anyhow!("invalid benchmark overlay"))?,
                );
            }
            indexes.push(VolumeIndex {
                volume: format!("benchmark-{}", indexes.len()),
                store: Arc::new(arc_swap::ArcSwap::from_pointee(view)),
                cursor: None,
            });
        }
        return serve_indexes(
            Arc::new(indexes),
            options.benchmark_pipe.as_deref().unwrap(),
        )
        .map_err(Into::into);
    }

    if let Some(path) = options.benchmark_cache {
        let scratch = std::env::current_dir()?.join(".scratch");
        std::fs::create_dir_all(&scratch)?;
        let absolute = std::path::absolute(&path)?;
        anyhow::ensure!(
            absolute.starts_with(&scratch),
            "live cache must be inside .scratch"
        );
        let parent = absolute
            .parent()
            .ok_or_else(|| anyhow::anyhow!("invalid cache directory"))?;
        std::fs::create_dir_all(parent)?;
        anyhow::ensure!(
            parent.canonicalize()?.starts_with(scratch.canonicalize()?),
            "live cache escapes scratch"
        );
        std::fs::create_dir(&absolute)?; // A fresh directory guarantees no snapshot resume.
        BENCHMARK_CACHE
            .set(absolute)
            .map_err(|_| anyhow::anyhow!("cache already configured"))?;
    }

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
    shutdown::listen()?;
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

    serve_indexes(
        indexes,
        options
            .benchmark_pipe
            .as_deref()
            .unwrap_or(scry_ipc::PIPE_NAME),
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests;
