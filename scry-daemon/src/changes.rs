//! Classify journal events and exclude every daemon-created file.

use crate::paths::{compaction_scratch_dir, snapshot_path};
use scry_core::delta::DeltaEvent;

/// The volume a snapshot file physically lives on, which is not necessarily
/// the volume it describes: snapshots are written under `%LOCALAPPDATA%`, so
/// a daemon indexing `D:` still writes that snapshot onto whatever drive the
/// user profile is on. `FSCTL_MARK_HANDLE` must be applied against the
/// hosting volume's journal, or the write is invisible to the auxiliary
/// filter and retriggers that volume's own watcher.
pub(super) fn hosting_volume(path: &std::path::Path) -> Option<String> {
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
pub(super) fn snapshot_file_names(volume: &str) -> [String; 4] {
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
pub(super) fn compaction_scratch_names(volume: &str) -> Vec<String> {
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
pub(super) fn owned_snapshot_names(
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
pub(super) struct SelfWriteFilter {
    own_names: std::collections::HashSet<String>,
    self_frns: std::collections::HashSet<u64>,
    use_auxiliary: bool,
}

impl SelfWriteFilter {
    pub(super) fn new(watched_volume: &str, all_volumes: &[String], use_auxiliary: bool) -> Self {
        SelfWriteFilter {
            own_names: owned_snapshot_names(watched_volume, all_volumes),
            self_frns: std::collections::HashSet::new(),
            use_auxiliary,
        }
    }

    pub(super) fn is_own_name(&self, name: &str) -> bool {
        self.own_names.contains(name)
    }
}

// The snapshot file itself lives on the volume being watched, so writing it
// produces USN events that would otherwise feed straight back into
// `reindex_on_changes` — an infinite reindex loop that never settles. When
// auxiliary marking is enabled, `is_auxiliary` alone identifies these
// exactly. The name/FRN heuristic stays as a fallback for when
// SeManageVolumePrivilege couldn't be enabled.
pub(super) fn is_real_change(ev: &scry_fsevents::ChangeEvent, state: &mut SelfWriteFilter) -> bool {
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

pub(super) fn collect_change(
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

pub(super) fn delta_event(event: &scry_fsevents::ChangeEvent) -> Option<DeltaEvent> {
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
