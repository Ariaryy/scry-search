//! Coalesced journal updates, view publication, and idle persistence.

use scry_core::delta::ApplyOutcome;
use scry_core::IndexView;
use std::sync::Arc;

use crate::changes::{collect_change, delta_event, SelfWriteFilter};
use crate::persistence::{build_view, compact_view, replay_exceeds_compaction_threshold};
use crate::platform::{configure_background_thread_qos, trim_working_set};
use crate::SharedStore;

/// Start one blocking journal reader and its bounded update worker.
pub(super) fn spawn_volume_watcher(
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

/// Polling is cheap; persisting is not. A cursor-only advance used to rewrite
/// the full snapshot after every 30-second quiet gap, turning ordinary file
/// activity into repeated index-sized bursts. Structural changes get a much
/// shorter durability bound than cursor-only progress, while clean shutdown
/// and the 5% compaction threshold remain immediate write points.
pub(super) const IDLE_PERSIST_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);
pub(super) const STRUCTURAL_PERSIST_QUIET_PERIOD: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);
pub(super) const CURSOR_CHECKPOINT_MAX_AGE: std::time::Duration =
    std::time::Duration::from_secs(60 * 60);
pub(super) const IDLE_PERSIST_RETRY_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);

#[derive(Default)]
pub(super) struct IdlePersistSchedule {
    dirty_since: Option<std::time::Instant>,
    last_structural_change: Option<std::time::Instant>,
    last_attempt: Option<std::time::Instant>,
}

impl IdlePersistSchedule {
    pub(super) fn observe(
        &mut self,
        now: std::time::Instant,
        structural_change: bool,
        cursor_advanced: bool,
    ) {
        if structural_change || cursor_advanced {
            self.dirty_since.get_or_insert(now);
        }
        if structural_change {
            self.last_structural_change = Some(now);
        }
    }

    pub(super) fn should_attempt(&self, now: std::time::Instant) -> bool {
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

    pub(super) fn attempted(&mut self, now: std::time::Instant) {
        self.last_attempt = Some(now);
    }

    pub(super) fn persisted(&mut self) {
        *self = Self::default();
    }
}

pub(super) fn view_has_structural_delta(view: &IndexView) -> bool {
    view.delta.tombstones.count_ones() != 0 || !view.delta.added.is_empty()
}

pub(super) fn view_needs_persist(view: &IndexView) -> bool {
    let archived = view.base.archived();
    view_has_structural_delta(view)
        || view.journal_id != archived.journal_id
        || view.next_usn != archived.next_usn
        || view.volume_serial != archived.volume_serial
}

pub(super) fn reindex_on_changes(
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
pub(super) fn persist_idle_view(
    store: &SharedStore,
    volume: &str,
    auxiliary_marking_enabled: bool,
) -> bool {
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
