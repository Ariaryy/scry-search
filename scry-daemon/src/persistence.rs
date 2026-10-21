//! Snapshot build, replay, and spool-backed compaction.

use scry_core::delta::ApplyOutcome;
use scry_core::{ArenaStore, IndexView};
use std::sync::Arc;

use crate::changes::{delta_event, hosting_volume, is_real_change, SelfWriteFilter};
use crate::metrics::process_memory;
use crate::paths::{compaction_scratch_dir, snapshot_path};
use crate::platform::BackgroundModeGuard;
use crate::StartupView;

pub(super) fn build_or_resume_view(
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
pub(super) fn replay_exceeds_compaction_threshold(event_count: usize, base_len: usize) -> bool {
    event_count.saturating_mul(20) > base_len
}

pub(super) fn resume_view(
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

pub(super) fn build_view(
    volume: &str,
    auxiliary_marking_enabled: bool,
) -> anyhow::Result<StartupView> {
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

pub(super) fn compact_view(
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
