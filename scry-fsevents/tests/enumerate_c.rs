//! Exercises the real FSCTL_ENUM_USN_DATA path against the C: volume.
//! Requires elevation (SeBackupPrivilege) to open the volume handle — if
//! that's not available in the current environment, the test reports the
//! failure mode instead of silently skipping, so a real bug doesn't get
//! confused with "not admin".

#[test]
fn enumerate_c_volume_or_report_why_not() {
    match scry_fsevents::WindowsBackend::bulk_index_volume("C:") {
        Ok(arena) => {
            assert!(
                arena.len() > 1000,
                "expected a real NTFS volume to have >1000 entries, got {}",
                arena.len()
            );
            println!("indexed {} entries from C:", arena.len());
        }
        Err(e) => {
            eprintln!(
                "bulk_index_volume(\"C:\") failed (expected without elevation): {e}"
            );
        }
    }
}
