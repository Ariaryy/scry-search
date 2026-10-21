use super::*;

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
