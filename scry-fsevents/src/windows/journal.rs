//! Journal cursor validation, replay, and blocking live watching.

use super::records::{decode_name_into, for_each_usn_record};
use super::{
    ffi, ioctl_overlapped, open_volume, to_wide, ChangeEvent, JournalCursor, WindowsBackend,
    WindowsBackendError,
};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

pub(super) fn query_journal(
    handle: ffi::Handle,
) -> Result<ffi::UsnJournalDataV0, WindowsBackendError> {
    let mut out = ffi::UsnJournalDataV0::default();
    let result = unsafe {
        ioctl_overlapped(
            handle,
            ffi::FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut out as *mut _ as *mut c_void,
            std::mem::size_of::<ffi::UsnJournalDataV0>() as u32,
            std::ptr::null_mut(),
        )
    };
    result
        .map(|_| out)
        .map_err(|code| WindowsBackendError::QueryJournal { code })
}

pub(super) fn journal_cursor(volume: &str) -> Result<JournalCursor, WindowsBackendError> {
    let handle = open_volume(volume, 0)?;
    let journal = query_journal(handle);
    unsafe { ffi::CloseHandle(handle) };
    let journal = journal?;

    let root = format!("{volume}\\");
    let wide = to_wide(&root);
    let mut serial = 0u32;
    let ok = unsafe {
        ffi::GetVolumeInformationW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return Err(WindowsBackendError::VolumeInformation {
            code: unsafe { ffi::GetLastError() },
        });
    }
    Ok(JournalCursor {
        journal_id: journal.usn_journal_id,
        first_usn: journal.first_usn,
        next_usn: journal.next_usn,
        volume_serial: serial.into(),
    })
}

pub(super) fn replay_journal(
    volume: &str,
    cursor: JournalCursor,
) -> Result<(JournalCursor, Vec<ChangeEvent>), WindowsBackendError> {
    let handle = open_volume(volume, 0)?;
    let result = (|| {
        let journal = query_journal(handle)?;
        let current = JournalCursor {
            journal_id: journal.usn_journal_id,
            first_usn: journal.first_usn,
            next_usn: journal.next_usn,
            volume_serial: volume_serial(volume)?,
        };
        validate_replay_cursor(cursor, current)?;

        let mut start_usn = cursor.next_usn;
        let mut out_buf = vec![0u8; 64 * 1024];
        let mut name = String::new();
        let mut events = Vec::new();

        while start_usn < current.next_usn {
            let input = ffi::ReadUsnJournalDataV0 {
                start_usn,
                reason_mask: ffi::USN_STRUCTURAL_REASONS,
                return_only_on_close: 0,
                timeout: 0,
                bytes_to_wait_for: 0,
                usn_journal_id: current.journal_id,
            };
            let bytes_returned = unsafe {
                ioctl_overlapped(
                    handle,
                    ffi::FSCTL_READ_USN_JOURNAL,
                    &input as *const _ as *const c_void,
                    std::mem::size_of::<ffi::ReadUsnJournalDataV0>() as u32,
                    out_buf.as_mut_ptr() as *mut c_void,
                    out_buf.len() as u32,
                    std::ptr::null_mut(),
                )
            }
            .map_err(|code| WindowsBackendError::ReadJournal { code })?;
            if bytes_returned < 8 {
                return Err(WindowsBackendError::ReadJournal {
                    code: ffi::ERROR_INVALID_DATA,
                });
            }
            let next_usn = i64::from_ne_bytes(out_buf[0..8].try_into().unwrap());
            if next_usn <= start_usn {
                return Err(WindowsBackendError::ReadJournal {
                    code: ffi::ERROR_INVALID_DATA,
                });
            }
            for_each_usn_record(&out_buf, bytes_returned as usize, |header, name_bytes| {
                if header.usn >= current.next_usn {
                    return;
                }
                decode_name_into(name_bytes, &mut name);
                events.push(classify(header, name.clone()));
            });
            start_usn = next_usn;
        }
        Ok((current, events))
    })();
    unsafe { ffi::CloseHandle(handle) };
    result
}

pub(super) fn validate_replay_cursor(
    cursor: JournalCursor,
    current: JournalCursor,
) -> Result<(), WindowsBackendError> {
    if cursor.volume_serial != current.volume_serial || cursor.journal_id != current.journal_id {
        return Err(WindowsBackendError::JournalChanged);
    }
    if cursor.next_usn < current.first_usn {
        return Err(WindowsBackendError::JournalAgedOut);
    }
    if cursor.next_usn > current.next_usn {
        return Err(WindowsBackendError::JournalChanged);
    }
    Ok(())
}

pub(super) fn volume_serial(volume: &str) -> Result<u64, WindowsBackendError> {
    let root = format!("{volume}\\");
    let wide = to_wide(&root);
    let mut serial = 0;
    let ok = unsafe {
        ffi::GetVolumeInformationW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return Err(WindowsBackendError::VolumeInformation {
            code: unsafe { ffi::GetLastError() },
        });
    }
    Ok(serial.into())
}

/// Blocks the calling thread, translating USN journal change records into
/// [`ChangeEvent`]s on `tx` until `should_stop` is set or `tx`'s receiver is
/// dropped. Starts from "now" (the journal's current USN) — it does not
/// replay history, so callers must complete a bulk index first.
///
/// The wait between changes is a genuinely blocking overlapped read: the
/// thread costs 0% CPU while the volume is idle. `watch_handle` publishes the
/// open volume handle so `JournalHandle::stop` can call `CancelIoEx` on it
/// from the stopping thread, which is what makes the wait interruptible
/// without polling.
/// Whether a raw USN `Reason` bitmask includes at least one reason that can
/// change the arena's tree shape (create/delete/rename). Exposed publicly so
/// the reason-mask narrowing can be unit tested without reaching into `ffi`.
pub fn is_structural_reason(reason: u32) -> bool {
    reason & ffi::USN_STRUCTURAL_REASONS != 0
}

pub(super) fn watch(
    volume: &str,
    start_cursor: Option<JournalCursor>,
    tx: &crossbeam::channel::Sender<ChangeEvent>,
    should_stop: &AtomicBool,
    watch_handle: &AtomicUsize,
    overflowed: &AtomicBool,
) -> Result<(), WindowsBackendError> {
    let handle = open_volume(volume, ffi::FILE_FLAG_OVERLAPPED)?;
    watch_handle.store(handle as usize, Ordering::Release);

    let result = (|| {
        let journal = query_journal(handle)?;
        let current = JournalCursor {
            journal_id: journal.usn_journal_id,
            first_usn: journal.first_usn,
            next_usn: journal.next_usn,
            volume_serial: volume_serial(volume)?,
        };
        let mut start_usn = match start_cursor {
            Some(cursor) => {
                validate_replay_cursor(cursor, current)?;
                cursor.next_usn
            }
            None => current.next_usn,
        };
        let mut out_buf = vec![0u8; 64 * 1024];
        let mut name = String::new();
        let mut events = Vec::new();

        while !should_stop.load(Ordering::Relaxed) {
            let input = ffi::ReadUsnJournalDataV0 {
                start_usn,
                reason_mask: ffi::USN_STRUCTURAL_REASONS,
                return_only_on_close: 1,
                // Block indefinitely for the next structural change.
                // CancelIoEx (from JournalHandle::stop) is what unblocks
                // this on shutdown, surfacing as ERROR_OPERATION_ABORTED.
                timeout: 0,
                bytes_to_wait_for: std::mem::size_of::<ffi::UsnRecordV2Header>() as u64,
                usn_journal_id: journal.usn_journal_id,
            };

            let bytes_returned = unsafe {
                ioctl_overlapped(
                    handle,
                    ffi::FSCTL_READ_USN_JOURNAL,
                    &input as *const _ as *const c_void,
                    std::mem::size_of::<ffi::ReadUsnJournalDataV0>() as u32,
                    out_buf.as_mut_ptr() as *mut c_void,
                    out_buf.len() as u32,
                    std::ptr::null_mut(),
                )
            };
            let bytes_returned = match bytes_returned {
                Ok(n) => n,
                Err(ffi::ERROR_OPERATION_ABORTED) => break, // normal shutdown
                Err(code) => return Err(WindowsBackendError::ReadJournal { code }),
            };
            if bytes_returned < 8 {
                continue;
            }

            let next_usn = i64::from_ne_bytes(out_buf[0..8].try_into().unwrap());
            start_usn = next_usn;

            events.clear();
            for_each_usn_record(&out_buf, bytes_returned as usize, |header, name_bytes| {
                decode_name_into(name_bytes, &mut name);
                events.push(classify(header, name.clone()));
            });
            for event in events.drain(..) {
                match tx.try_send(event) {
                    Ok(()) => {}
                    Err(crossbeam::channel::TrySendError::Disconnected(_)) => {
                        return Ok(()); // receiver gone, nothing left to do
                    }
                    Err(crossbeam::channel::TrySendError::Full(_)) => {
                        // The daemon is mid-reindex and can't keep up. Dropping
                        // individual events would silently desync the index, so
                        // instead record that a full resync is required; the
                        // consumer treats the flag as "reindex regardless".
                        overflowed.store(true, Ordering::Relaxed);
                    }
                }
            }
            if let Err(error) = tx.try_send(ChangeEvent::Advanced { next_usn }) {
                match error {
                    crossbeam::channel::TrySendError::Disconnected(_) => break,
                    crossbeam::channel::TrySendError::Full(_) => {
                        overflowed.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        Ok(())
    })();
    unsafe { ffi::CloseHandle(handle) };
    watch_handle.store(0, Ordering::Release);
    result
}

pub(super) fn classify(header: &ffi::UsnRecordV2Header, name: String) -> ChangeEvent {
    let reason = header.reason;
    let is_auxiliary = header.source_info & ffi::USN_SOURCE_AUXILIARY_DATA != 0;
    if reason & ffi::USN_REASON_FILE_DELETE != 0 {
        ChangeEvent::Deleted {
            frn: header.file_reference_number,
            is_auxiliary,
        }
    } else if reason & ffi::USN_REASON_FILE_CREATE != 0 {
        ChangeEvent::Created {
            frn: header.file_reference_number,
            parent_frn: header.parent_file_reference_number,
            name,
            is_dir: header.file_attributes & ffi::FILE_ATTRIBUTE_DIRECTORY != 0,
            is_auxiliary,
        }
    } else if reason & ffi::USN_REASON_RENAME_NEW_NAME != 0 {
        ChangeEvent::Renamed {
            frn: header.file_reference_number,
            parent_frn: header.parent_file_reference_number,
            name,
            is_auxiliary,
        }
    } else {
        // With the narrowed mask, only RENAME_OLD_NAME reaches here — the
        // matching RENAME_NEW_NAME record carries the information the
        // daemon actually needs.
        ChangeEvent::Modified {
            frn: header.file_reference_number,
            is_auxiliary,
        }
    }
}

/// Handle to a background journal-watcher thread. Dropping this without
/// calling `stop` leaves the thread running detached — call `stop` to shut
/// it down deterministically (e.g. on daemon exit).
pub struct JournalHandle {
    stop: Arc<AtomicBool>,
    // The open volume handle, published by `watch()` once it opens it and
    // cleared back to 0 when it closes it. Stored as a `usize` rather than
    // `ffi::Handle` (`*mut c_void`) so this struct stays `Send` without an
    // unsafe impl — see the same pattern in scry-ipc's `SecurityDescriptor`.
    watch_handle: Arc<AtomicUsize>,
    overflowed: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<Result<(), WindowsBackendError>>>,
}

impl JournalHandle {
    /// Returns whether the event channel filled up since the last call, and
    /// resets the flag. Callers must treat a `true` result as "a structural
    /// change may have been dropped" and force a full reindex.
    pub fn take_overflow(&self) -> bool {
        self.overflowed.swap(false, Ordering::Relaxed)
    }

    pub fn stop(mut self) -> Result<(), WindowsBackendError> {
        self.stop.store(true, Ordering::Relaxed);
        let handle_val = self.watch_handle.load(Ordering::Acquire);
        if handle_val != 0 {
            // Unblocks the pending overlapped read in watch() with
            // ERROR_OPERATION_ABORTED instead of leaving it to wait for the
            // next real journal event, which might never come.
            unsafe {
                ffi::CancelIoEx(handle_val as ffi::Handle, std::ptr::null());
            }
        }
        match self.join.take().unwrap().join() {
            Ok(res) => res,
            Err(_) => Ok(()), // watcher thread panicked; nothing more to report here
        }
    }
}

impl WindowsBackend {
    /// Spawns a background thread streaming live USN journal changes for
    /// `volume` into `tx`. Pair with `bulk_index_volume` for the initial
    /// snapshot — this only reports changes from the moment it starts.
    pub fn spawn_watcher(
        volume: &str,
        tx: crossbeam::channel::Sender<ChangeEvent>,
    ) -> JournalHandle {
        Self::spawn_watcher_from(volume, None, tx)
    }

    /// Spawns a watcher at a cursor captured before the base enumeration or
    /// returned by replay. This closes the interval between producing a view
    /// and starting its live updates.
    pub fn spawn_watcher_from(
        volume: &str,
        cursor: Option<JournalCursor>,
        tx: crossbeam::channel::Sender<ChangeEvent>,
    ) -> JournalHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let watch_handle = Arc::new(AtomicUsize::new(0));
        let watch_handle_thread = watch_handle.clone();
        let overflowed = Arc::new(AtomicBool::new(false));
        let overflowed_thread = overflowed.clone();
        let volume = volume.to_string();
        let join = std::thread::spawn(move || {
            watch(
                &volume,
                cursor,
                &tx,
                &stop_thread,
                &watch_handle_thread,
                &overflowed_thread,
            )
        });
        JournalHandle {
            stop,
            watch_handle,
            overflowed,
            join: Some(join),
        }
    }
}
