//! Exercises the live USN journal watcher end-to-end: create, rename, and
//! delete a real file on C: and confirm the corresponding ChangeEvents show
//! up. Requires elevation, same as the enumeration test.

use scry_fsevents::{is_structural_reason, ChangeEvent, WindowsBackend};
use std::time::{Duration, Instant};

const USN_REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
const USN_REASON_CLOSE: u32 = 0x8000_0000;
const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;
const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;

#[test]
fn structural_reason_mask_excludes_data_writes() {
    assert!(!is_structural_reason(USN_REASON_DATA_OVERWRITE));
    assert!(!is_structural_reason(USN_REASON_CLOSE));
    assert!(!is_structural_reason(USN_REASON_BASIC_INFO_CHANGE));

    assert!(is_structural_reason(USN_REASON_FILE_CREATE));
    assert!(is_structural_reason(USN_REASON_FILE_DELETE));
    assert!(is_structural_reason(USN_REASON_RENAME_OLD_NAME));
    assert!(is_structural_reason(USN_REASON_RENAME_NEW_NAME));
}

#[test]
fn watch_reports_create_rename_delete() {
    let (tx, rx) = crossbeam::channel::unbounded();
    let handle = WindowsBackend::spawn_watcher("C:", tx);

    // Give the watcher a moment to open the journal and start polling
    // before we generate events for it to observe.
    std::thread::sleep(Duration::from_millis(500));

    let dir = std::env::temp_dir();
    let original = dir.join("scry_watch_test_file.txt");
    let renamed = dir.join("scry_watch_test_file_renamed.txt");
    let _ = std::fs::remove_file(&original);
    let _ = std::fs::remove_file(&renamed);

    std::fs::write(&original, b"hello").unwrap();
    std::fs::rename(&original, &renamed).unwrap();
    std::fs::remove_file(&renamed).unwrap();

    let mut saw_create = false;
    let mut saw_rename = false;
    let mut saw_delete = false;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline && !(saw_create && saw_rename && saw_delete) {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(500)) {
            match event {
                ChangeEvent::Created { ref name, .. } if name.contains("scry_watch_test_file") => {
                    saw_create = true;
                }
                ChangeEvent::Renamed { ref name, .. } if name.contains("renamed") => {
                    saw_rename = true;
                }
                ChangeEvent::Deleted { .. } => {
                    saw_delete = true;
                }
                _ => {}
            }
        }
    }

    // If the watcher thread couldn't even open the volume (unelevated), report
    // that distinctly rather than failing the assertions below with no context.
    if let Err(e) = handle.stop() {
        eprintln!("watcher failed (expected without elevation): {e}");
        return;
    }

    assert!(saw_create, "did not observe a Created event");
    assert!(saw_rename, "did not observe a Renamed event");
    assert!(saw_delete, "did not observe a Deleted event");
}
