//! Windows backend: bulk-load the index via `FSCTL_ENUM_USN_DATA`, which walks
//! the NTFS MFT in on-disk (record) order rather than the directory tree —
//! turning "enumerate a million files" from a million small directory-read
//! syscalls into a handful of large sequential reads of the MFT itself.
//!
//! Live updates ride the same USN journal via `FSCTL_READ_USN_JOURNAL`, using
//! overlapped I/O so the watcher thread blocks at 0% CPU when the volume is
//! idle; `JournalHandle::stop` unblocks it deterministically via `CancelIoEx`.

use scry_core::record::filetime_to_secs;
use scry_core::ArenaBuilder;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum WindowsBackendError {
    #[error("failed to open volume {volume}: win32 error {code}")]
    OpenVolume { volume: String, code: u32 },
    #[error("DeviceIoControl(FSCTL_ENUM_USN_DATA) failed: win32 error {code}")]
    Enumerate { code: u32 },
    #[error("DeviceIoControl(FSCTL_QUERY_USN_JOURNAL) failed: win32 error {code}")]
    QueryJournal { code: u32 },
    #[error("DeviceIoControl(FSCTL_READ_USN_JOURNAL) failed: win32 error {code}")]
    ReadJournal { code: u32 },
}

/// A change observed on the volume's USN journal, coarsened down to what the
/// daemon's index-update thread actually needs to patch the Arena.
#[derive(Debug, Clone)]
pub enum ChangeEvent {
    Created {
        frn: u64,
        parent_frn: u64,
        name: String,
        is_dir: bool,
        /// Set when this record was produced by a handle marked via
        /// `FSCTL_MARK_HANDLE` — i.e. it's scry's own write, not a real
        /// filesystem change. See `mark_handle_as_auxiliary`.
        is_auxiliary: bool,
    },
    Deleted {
        frn: u64,
        is_auxiliary: bool,
    },
    /// Covers both a rename and a move (new parent), since NTFS reports both
    /// as a name-change record on the same FRN.
    Renamed {
        frn: u64,
        parent_frn: u64,
        name: String,
        is_auxiliary: bool,
    },
    /// Metadata/content changed but identity, name and parent did not — the
    /// arena's tree shape is unaffected, so the daemon can ignore or use this
    /// only to refresh mtime.
    Modified {
        frn: u64,
        is_auxiliary: bool,
    },
}

impl ChangeEvent {
    pub fn is_auxiliary(&self) -> bool {
        match self {
            ChangeEvent::Created { is_auxiliary, .. }
            | ChangeEvent::Deleted { is_auxiliary, .. }
            | ChangeEvent::Renamed { is_auxiliary, .. }
            | ChangeEvent::Modified { is_auxiliary, .. } => *is_auxiliary,
        }
    }
}

pub struct WindowsBackend;

impl WindowsBackend {
    /// Enables a named privilege (e.g. `SeManageVolumePrivilege`) on the
    /// current process token. See the free function of the same name for
    /// details.
    pub fn enable_privilege(name: &str) -> Result<(), ffi::Dword> {
        enable_privilege(name)
    }

    /// Tags `file`'s handle so its writes are identifiable in the USN
    /// journal. See the free function of the same name for details.
    pub fn mark_handle_as_auxiliary(file: &std::fs::File, volume: &str) -> Result<(), ffi::Dword> {
        mark_handle_as_auxiliary(file, volume)
    }

    /// Bulk-enumerate a volume (e.g. `"C:"`) directly from its MFT via the USN
    /// journal's enumeration ioctl, bypassing per-file stat()/directory walks.
    /// Requires the process to hold `SeBackupPrivilege` (i.e. run elevated).
    pub fn bulk_index_volume(
        volume: &str,
    ) -> Result<(scry_core::Arena, Vec<scry_core::frnmap::FrnEntry>), WindowsBackendError> {
        // Arbitrary initial guess (200k entries / 4 MB of names) that avoids
        // the first several reallocations of the staging blob without
        // over-committing on small volumes.
        let mut builder = ArenaBuilder::with_capacity(200_000, 4 * 1024 * 1024);
        // (frn, provisional index), sorted after enumeration for binary
        // search. A hashmap keyed by frn costs ~18 MB at a million entries
        // once hashbrown's load factor is accounted for, with random-access
        // probe behaviour; a sorted Vec is 12 bytes per entry, contiguous,
        // and built with one sort.
        let mut frn_table: Vec<(u64, u32)> = Vec::new();
        // Parallel to record order: each entry's parent FRN, needed once all
        // entries (and therefore all indices) are known.
        let mut parent_frns: Vec<u64> = Vec::new();

        enumerate_mft(volume, |frn, parent_frn, name, is_dir, mtime| {
            let idx = builder.push_bytes_with_frn(name, filetime_to_secs(mtime), is_dir, frn);
            frn_table.push((frn, idx));
            parent_frns.push(parent_frn);
        })?;

        // The NTFS root record (self-parented, or simply never referenced by
        // any child's parent FRN, depending on what the enumeration yields)
        // otherwise contributes nothing to full_path() — every top-level
        // entry's parent chain would just stop, dropping the drive letter.
        // Give every "no real parent" entry an explicit volume-root node
        // so full_path() always includes the drive letter.
        let root_idx = builder.push(volume, 0, true);

        frn_table.sort_unstable_by_key(|&(frn, _)| frn);

        for (i, &parent_frn) in parent_frns.iter().enumerate() {
            let idx = i as u32;
            match frn_table.binary_search_by_key(&parent_frn, |&(frn, _)| frn) {
                Ok(pos) if frn_table[pos].1 != idx => builder.set_parent(idx, frn_table[pos].1),
                _ => builder.set_parent(idx, root_idx),
            }
        }

        // Released before build()'s sort allocates its permutation — ~20 MB
        // freed ahead of the next big transient.
        drop(frn_table);
        drop(parent_frns);

        Ok(builder.build())
    }
}

fn open_volume(volume: &str, flags: ffi::Dword) -> Result<ffi::Handle, WindowsBackendError> {
    let path = format!("\\\\.\\{volume}");
    let wide = to_wide(&path);

    let handle = unsafe {
        ffi::CreateFileW(
            wide.as_ptr(),
            ffi::GENERIC_READ,
            ffi::FILE_SHARE_READ | ffi::FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            ffi::OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if handle == ffi::INVALID_HANDLE_VALUE {
        let code = unsafe { ffi::GetLastError() };
        return Err(WindowsBackendError::OpenVolume {
            volume: volume.to_string(),
            code,
        });
    }
    Ok(handle)
}

/// Enables a named privilege (e.g. `SeManageVolumePrivilege`) on the current
/// process token. Administrators tokens hold this privilege but it is
/// disabled by default; `AdjustTokenPrivileges` returns nonzero even when it
/// only partially succeeds, so success must be confirmed separately via
/// `GetLastError() != ERROR_NOT_ALL_ASSIGNED`.
pub fn enable_privilege(name: &str) -> Result<(), ffi::Dword> {
    unsafe {
        let mut token: ffi::Handle = std::ptr::null_mut();
        let process = ffi::GetCurrentProcess();
        if ffi::OpenProcessToken(
            process,
            ffi::TOKEN_ADJUST_PRIVILEGES | ffi::TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return Err(ffi::GetLastError());
        }

        let wide_name = to_wide(name);
        let mut luid = ffi::Luid {
            low_part: 0,
            high_part: 0,
        };
        if ffi::LookupPrivilegeValueW(std::ptr::null(), wide_name.as_ptr(), &mut luid) == 0 {
            let err = ffi::GetLastError();
            ffi::CloseHandle(token);
            return Err(err);
        }

        let privileges = ffi::TokenPrivileges {
            privilege_count: 1,
            privileges: ffi::LuidAndAttributes {
                luid,
                attributes: ffi::SE_PRIVILEGE_ENABLED,
            },
        };
        let ok = ffi::AdjustTokenPrivileges(
            token,
            0,
            &privileges,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let err = ffi::GetLastError();
        ffi::CloseHandle(token);
        if ok == 0 {
            return Err(err);
        }
        if err == ffi::ERROR_NOT_ALL_ASSIGNED {
            return Err(err);
        }
        Ok(())
    }
}

/// Tags `file`'s handle via `FSCTL_MARK_HANDLE` so that any USN records
/// produced by writes through this handle carry `USN_SOURCE_AUXILIARY_DATA`
/// in `SourceInfo`, letting the journal watcher recognize the daemon's own
/// snapshot writes exactly instead of matching them by name/FRN. Requires
/// `SeManageVolumePrivilege` to already be enabled (see `enable_privilege`);
/// callers should treat failure as non-fatal and fall back to the name-based
/// heuristic.
pub fn mark_handle_as_auxiliary(file: &std::fs::File, volume: &str) -> Result<(), ffi::Dword> {
    use std::os::windows::io::AsRawHandle;

    let volume_handle = open_volume(volume, 0).map_err(|_| unsafe { ffi::GetLastError() })?;

    let mut info = ffi::MarkHandleInfo {
        usn_source_info: ffi::USN_SOURCE_AUXILIARY_DATA,
        _pad0: 0,
        volume_handle,
        handle_info: 0,
        _pad1: 0,
    };

    let result = unsafe {
        let mut bytes_returned: u32 = 0;
        let ok = ffi::DeviceIoControl(
            file.as_raw_handle() as ffi::Handle,
            ffi::FSCTL_MARK_HANDLE,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ffi::MarkHandleInfo>() as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        );
        if ok != 0 {
            Ok(())
        } else {
            Err(ffi::GetLastError())
        }
    };

    unsafe { ffi::CloseHandle(volume_handle) };
    result
}

/// Bulk-enumerates the volume's MFT, handing each entry to `sink` as it is
/// parsed. Streaming rather than returning a `Vec<RawEntry>` is what keeps a
/// rebuild from holding a `String` per file: the caller copies the name into
/// its own staging blob and the ioctl buffer is immediately reused.
///
/// `sink` receives: FRN, parent FRN, UTF-8 name bytes, is-directory, and the
/// raw FILETIME mtime.
fn enumerate_mft(
    volume: &str,
    mut sink: impl FnMut(u64, u64, &[u8], bool, i64),
) -> Result<(), WindowsBackendError> {
    let handle = open_volume(volume, 0)?;

    let result = (|| {
        let mut start_frn: u64 = 0;
        // 1 MiB output buffer. The ioctl's per-call cost is a kernel
        // transition plus MFT seek setup, so a larger buffer directly
        // reduces syscall count and wall time on a full enumeration. One
        // megabyte is immaterial against the ~14 MB steady-state index; the
        // earlier 64 KiB choice was optimising the wrong side of that
        // tradeoff.
        let mut out_buf = vec![0u8; 1024 * 1024];
        let mut name = String::new();

        loop {
            let input = ffi::MftEnumDataV0 {
                start_file_reference_number: start_frn,
                low_usn: 0,
                high_usn: i64::MAX,
            };
            let mut bytes_returned: u32 = 0;

            let ok = unsafe {
                ffi::DeviceIoControl(
                    handle,
                    ffi::FSCTL_ENUM_USN_DATA,
                    &input as *const _ as *const c_void,
                    std::mem::size_of::<ffi::MftEnumDataV0>() as u32,
                    out_buf.as_mut_ptr() as *mut c_void,
                    out_buf.len() as u32,
                    &mut bytes_returned,
                    std::ptr::null_mut(),
                )
            };

            if ok == 0 {
                let code = unsafe { ffi::GetLastError() };
                if code == ffi::ERROR_HANDLE_EOF {
                    break;
                }
                return Err(WindowsBackendError::Enumerate { code });
            }
            if bytes_returned < 8 {
                break;
            }

            // First 8 bytes: FRN to resume from on the next call.
            let next_start = u64::from_ne_bytes(out_buf[0..8].try_into().unwrap());

            for_each_usn_record(&out_buf, bytes_returned as usize, |header, name_bytes| {
                decode_name_into(name_bytes, &mut name);
                sink(
                    header.file_reference_number,
                    header.parent_file_reference_number,
                    name.as_bytes(),
                    header.file_attributes & ffi::FILE_ATTRIBUTE_DIRECTORY != 0,
                    header.time_stamp,
                );
            });

            if next_start == start_frn {
                break;
            }
            start_frn = next_start;
        }

        Ok(())
    })();

    unsafe { ffi::CloseHandle(handle) };
    result
}

/// Walks the USN_RECORD_V2 entries in an ioctl output buffer, handing each
/// header and its raw UTF-16LE filename bytes to `f` without allocating.
///
/// The previous signature returned `Vec<(Header, String)>`, which allocated a
/// vector and a `String` per record per 64 KiB buffer — tens of millions of
/// transient allocations across a full MFT enumeration.
///
/// Both `FSCTL_ENUM_USN_DATA` and `FSCTL_READ_USN_JOURNAL` share this exact
/// wire format (an 8-byte cursor followed by packed variable-length records),
/// so enumeration and live-journal reads both use this.
fn for_each_usn_record(
    buf: &[u8],
    bytes_returned: usize,
    mut f: impl FnMut(&ffi::UsnRecordV2Header, &[u8]),
) {
    let mut offset = 8usize;
    while offset + std::mem::size_of::<ffi::UsnRecordV2Header>() <= bytes_returned {
        let header: ffi::UsnRecordV2Header =
            unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr() as *const _) };
        if header.record_length == 0 {
            break;
        }
        // A malformed record_length smaller than the header itself would
        // make `offset` fail to advance and spin forever.
        if (header.record_length as usize) < std::mem::size_of::<ffi::UsnRecordV2Header>() {
            break;
        }

        // `buf` is the full ioctl allocation, not the valid region — the
        // bound must be `bytes_returned`, not `buf.len()`. Checking against
        // `buf.len()` let a truncated final record read stale bytes left
        // over from a previous call and yield a garbage filename instead of
        // being skipped.
        let record_end = offset + header.record_length as usize;
        if record_end > bytes_returned {
            break;
        }
        let name_start = offset + header.file_name_offset as usize;
        let name_end = name_start + header.file_name_length as usize;
        if name_end > record_end {
            break;
        }

        f(&header, &buf[name_start..name_end]);
        offset = record_end;
    }
}

/// Decodes UTF-16LE name bytes into `out` (cleared first) as UTF-8, replacing
/// unpaired surrogates. NTFS filenames are UCS-2 and may contain unpaired
/// surrogates, so lossy conversion is correct, not a shortcut.
fn decode_name_into(name_bytes: &[u8], out: &mut String) {
    out.clear();
    let utf16: Vec<u16> = name_bytes
        .chunks_exact(2)
        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
        .collect();
    out.push_str(&String::from_utf16_lossy(&utf16));
}

/// Issues a `DeviceIoControl` on a handle opened with `FILE_FLAG_OVERLAPPED`
/// and blocks the calling thread until it completes. `event_handle` may be
/// null, in which case the device handle itself is the completion signal —
/// sound as long as only one overlapped operation is ever outstanding on
/// `handle` at a time, which every caller in this module guarantees. A
/// completion that came from `CancelIoEx` surfaces as
/// `Err(ffi::ERROR_OPERATION_ABORTED)`, not a hang.
unsafe fn ioctl_overlapped(
    handle: ffi::Handle,
    code: ffi::Dword,
    input: *const c_void,
    input_len: u32,
    output: *mut c_void,
    output_len: u32,
    event_handle: ffi::Handle,
) -> Result<u32, ffi::Dword> {
    let mut overlapped = ffi::Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        h_event: event_handle,
    };
    let mut bytes_returned: u32 = 0;
    let ok = ffi::DeviceIoControl(
        handle,
        code,
        input,
        input_len,
        output,
        output_len,
        &mut bytes_returned,
        &mut overlapped as *mut _ as *mut c_void,
    );
    if ok != 0 {
        return Ok(bytes_returned);
    }
    let err = ffi::GetLastError();
    if err != ffi::ERROR_IO_PENDING {
        return Err(err);
    }
    let waited = ffi::GetOverlappedResult(handle, &overlapped, &mut bytes_returned, 1);
    if waited != 0 {
        Ok(bytes_returned)
    } else {
        Err(ffi::GetLastError())
    }
}

fn query_journal(handle: ffi::Handle) -> Result<ffi::UsnJournalDataV0, WindowsBackendError> {
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

fn watch(
    volume: &str,
    tx: &crossbeam::channel::Sender<ChangeEvent>,
    should_stop: &AtomicBool,
    watch_handle: &AtomicUsize,
    overflowed: &AtomicBool,
) -> Result<(), WindowsBackendError> {
    let handle = open_volume(volume, ffi::FILE_FLAG_OVERLAPPED)?;
    watch_handle.store(handle as usize, Ordering::Release);

    let result = (|| {
        let journal = query_journal(handle)?;
        let mut start_usn = journal.next_usn;
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
        }
        Ok(())
    })();
    unsafe { ffi::CloseHandle(handle) };
    watch_handle.store(0, Ordering::Release);
    result
}

fn classify(header: &ffi::UsnRecordV2Header, name: String) -> ChangeEvent {
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

fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Hand-rolled kernel32 FFI. USN_RECORD_V2 has a variable-length filename
/// tail that doesn't fit a fixed-size generated binding cleanly, so this
/// module owns the exact layout instead of depending on windows-sys's.
mod ffi {
    use std::ffi::c_void;

    pub type Handle = *mut c_void;
    pub type Bool = i32;
    pub type Dword = u32;

    pub const GENERIC_READ: Dword = 0x8000_0000;
    pub const FILE_SHARE_READ: Dword = 0x0000_0001;
    pub const FILE_SHARE_WRITE: Dword = 0x0000_0002;
    pub const OPEN_EXISTING: Dword = 3;
    pub const ERROR_HANDLE_EOF: Dword = 38;
    pub const FILE_ATTRIBUTE_DIRECTORY: Dword = 0x10;
    /// CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 44, METHOD_NEITHER, FILE_ANY_ACCESS)
    pub const FSCTL_ENUM_USN_DATA: Dword = 0x000900B3;
    /// CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 61, METHOD_BUFFERED, FILE_ANY_ACCESS)
    pub const FSCTL_QUERY_USN_JOURNAL: Dword = 0x0009_00F4;
    /// CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 46, METHOD_NEITHER, FILE_ANY_ACCESS)
    pub const FSCTL_READ_USN_JOURNAL: Dword = 0x0009_00BB;

    // USN_REASON_* — the subset scry cares about. A name-and-tree index is
    // only affected by records that create, destroy, or move an entry;
    // data writes (DATA_OVERWRITE/EXTEND, CLOSE, BASIC_INFO_CHANGE) cannot
    // change anything the index can answer.
    pub const USN_REASON_FILE_CREATE: Dword = 0x0000_0100;
    pub const USN_REASON_FILE_DELETE: Dword = 0x0000_0200;
    pub const USN_REASON_RENAME_OLD_NAME: Dword = 0x0000_1000;
    pub const USN_REASON_RENAME_NEW_NAME: Dword = 0x0000_2000;

    /// The only reasons that can alter the arena's shape. Passed as
    /// `ReadUsnJournalDataV0::reason_mask`, which returns a record when
    /// `record.reason & mask != 0`.
    pub const USN_STRUCTURAL_REASONS: Dword = USN_REASON_FILE_CREATE
        | USN_REASON_FILE_DELETE
        | USN_REASON_RENAME_OLD_NAME
        | USN_REASON_RENAME_NEW_NAME;

    /// CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 63, METHOD_BUFFERED, FILE_ANY_ACCESS)
    pub const FSCTL_MARK_HANDLE: Dword = 0x0009_00FC;
    /// Set in USN_RECORD_V2::source_info for records generated through a
    /// handle marked with this flag. Lets the daemon recognise its own
    /// snapshot writes without matching on filenames.
    pub const USN_SOURCE_AUXILIARY_DATA: Dword = 0x0000_0002;

    /// Input to FSCTL_MARK_HANDLE. `usn_source_info` is a union with
    /// `CopyNumber` in the C header; scry only ever uses the former.
    /// On x64 the HANDLE forces 8-byte alignment, so there is 4 bytes of
    /// padding after `usn_source_info` and 4 at the end — total size 24.
    #[repr(C)]
    pub struct MarkHandleInfo {
        pub usn_source_info: Dword,
        pub _pad0: Dword,
        pub volume_handle: Handle,
        pub handle_info: Dword,
        pub _pad1: Dword,
    }

    pub const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

    pub const FILE_FLAG_OVERLAPPED: Dword = 0x4000_0000;
    pub const ERROR_IO_PENDING: Dword = 997;
    pub const ERROR_OPERATION_ABORTED: Dword = 995;

    /// Win32 OVERLAPPED. The first four fields are a union in the C header
    /// (Internal/InternalHigh/Offset/OffsetHigh vs. Pointer); the layout below
    /// matches the Offset/OffsetHigh form, which is what DeviceIoControl uses.
    #[repr(C)]
    pub struct Overlapped {
        pub internal: usize,
        pub internal_high: usize,
        pub offset: u32,
        pub offset_high: u32,
        pub h_event: Handle,
    }

    #[repr(C)]
    pub struct MftEnumDataV0 {
        pub start_file_reference_number: u64,
        pub low_usn: i64,
        pub high_usn: i64,
    }

    /// Output of FSCTL_QUERY_USN_JOURNAL: identifies the active journal and
    /// its current cursor (`next_usn`), which is where live watching starts.
    #[repr(C)]
    #[derive(Default)]
    pub struct UsnJournalDataV0 {
        pub usn_journal_id: u64,
        pub first_usn: i64,
        pub next_usn: i64,
        pub lowest_valid_usn: i64,
        pub max_usn: i64,
        pub maximum_size: u64,
        pub allocation_delta: u64,
    }

    /// Input to FSCTL_READ_USN_JOURNAL.
    #[repr(C)]
    pub struct ReadUsnJournalDataV0 {
        pub start_usn: i64,
        pub reason_mask: u32,
        pub return_only_on_close: u32,
        pub timeout: u64,
        pub bytes_to_wait_for: u64,
        pub usn_journal_id: u64,
    }

    /// Fixed-size header of USN_RECORD_V2; the filename follows at
    /// `file_name_offset` bytes from the start of the record. Shared by both
    /// FSCTL_ENUM_USN_DATA and FSCTL_READ_USN_JOURNAL output.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct UsnRecordV2Header {
        pub record_length: u32,
        pub major_version: u16,
        pub minor_version: u16,
        pub file_reference_number: u64,
        pub parent_file_reference_number: u64,
        pub usn: i64,
        pub time_stamp: i64,
        pub reason: u32,
        pub source_info: u32,
        pub security_id: u32,
        pub file_attributes: u32,
        pub file_name_length: u16,
        pub file_name_offset: u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: Dword,
            dw_share_mode: Dword,
            lp_security_attributes: *mut c_void,
            dw_creation_disposition: Dword,
            dw_flags_and_attributes: Dword,
            h_template_file: Handle,
        ) -> Handle;

        pub fn CloseHandle(h_object: Handle) -> Bool;

        pub fn DeviceIoControl(
            h_device: Handle,
            dw_io_control_code: Dword,
            lp_in_buffer: *const c_void,
            n_in_buffer_size: Dword,
            lp_out_buffer: *mut c_void,
            n_out_buffer_size: Dword,
            lp_bytes_returned: *mut Dword,
            lp_overlapped: *mut c_void,
        ) -> Bool;

        pub fn GetLastError() -> Dword;

        pub fn GetOverlappedResult(
            h_file: Handle,
            lp_overlapped: *const Overlapped,
            lp_number_of_bytes_transferred: *mut Dword,
            b_wait: Bool,
        ) -> Bool;

        pub fn CancelIoEx(h_file: Handle, lp_overlapped: *const Overlapped) -> Bool;
    }

    pub const TOKEN_ADJUST_PRIVILEGES: Dword = 0x20;
    pub const TOKEN_QUERY: Dword = 0x8;
    pub const SE_PRIVILEGE_ENABLED: Dword = 0x2;
    pub const ERROR_NOT_ALL_ASSIGNED: Dword = 1300;

    #[repr(C)]
    pub struct Luid {
        pub low_part: u32,
        pub high_part: i32,
    }

    #[repr(C)]
    pub struct LuidAndAttributes {
        pub luid: Luid,
        pub attributes: Dword,
    }

    /// TOKEN_PRIVILEGES with a single trailing LUID_AND_ATTRIBUTES — the real
    /// struct has a variable-length `Privileges[ANYSIZE_ARRAY]` tail, but scry
    /// only ever adjusts one privilege at a time.
    #[repr(C)]
    pub struct TokenPrivileges {
        pub privilege_count: Dword,
        pub privileges: LuidAndAttributes,
    }

    #[link(name = "advapi32")]
    extern "system" {
        pub fn OpenProcessToken(
            process_handle: Handle,
            desired_access: Dword,
            token_handle: *mut Handle,
        ) -> Bool;

        pub fn LookupPrivilegeValueW(
            lp_system_name: *const u16,
            lp_name: *const u16,
            lp_luid: *mut Luid,
        ) -> Bool;

        pub fn AdjustTokenPrivileges(
            token_handle: Handle,
            disable_all_privileges: Bool,
            new_state: *const TokenPrivileges,
            buffer_length: Dword,
            previous_state: *mut TokenPrivileges,
            return_length: *mut Dword,
        ) -> Bool;
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetCurrentProcess() -> Handle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(reason: u32, source_info: u32) -> ffi::UsnRecordV2Header {
        ffi::UsnRecordV2Header {
            record_length: 0,
            major_version: 2,
            minor_version: 0,
            file_reference_number: 1,
            parent_file_reference_number: 2,
            usn: 0,
            time_stamp: 0,
            reason,
            source_info,
            security_id: 0,
            file_attributes: 0,
            file_name_length: 0,
            file_name_offset: 0,
        }
    }

    #[test]
    fn classify_marks_auxiliary_source() {
        let h = header(ffi::USN_REASON_FILE_CREATE, ffi::USN_SOURCE_AUXILIARY_DATA);
        let ChangeEvent::Created { is_auxiliary, .. } = classify(&h, "foo".to_string()) else {
            panic!("expected Created");
        };
        assert!(is_auxiliary);

        let h = header(ffi::USN_REASON_FILE_CREATE, 0);
        let ChangeEvent::Created { is_auxiliary, .. } = classify(&h, "foo".to_string()) else {
            panic!("expected Created");
        };
        assert!(!is_auxiliary);
    }

    const HEADER_SIZE: usize = std::mem::size_of::<ffi::UsnRecordV2Header>();

    /// Appends one well-formed USN_RECORD_V2 record (header + UTF-16LE name)
    /// to `buf` and returns the record's total byte length.
    fn append_record(buf: &mut Vec<u8>, name: &str) -> usize {
        let name_utf16: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let record_len = HEADER_SIZE + name_utf16.len();
        let h = ffi::UsnRecordV2Header {
            record_length: record_len as u32,
            major_version: 2,
            minor_version: 0,
            file_reference_number: 1,
            parent_file_reference_number: 2,
            usn: 0,
            time_stamp: 0,
            reason: ffi::USN_REASON_FILE_CREATE,
            source_info: 0,
            security_id: 0,
            file_attributes: 0,
            file_name_length: name_utf16.len() as u16,
            file_name_offset: HEADER_SIZE as u16,
        };
        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                (&h as *const ffi::UsnRecordV2Header) as *const u8,
                HEADER_SIZE,
            )
        };
        buf.extend_from_slice(header_bytes);
        buf.extend_from_slice(&name_utf16);
        record_len
    }

    #[test]
    fn truncated_trailing_record_is_skipped_not_misread() {
        let mut buf = vec![0u8; 8]; // 8-byte resume cursor
        append_record(&mut buf, "valid.txt");

        // A second header claiming a record_length that extends past the
        // buffer's valid region (bytes_returned) — must be skipped, not read.
        let bad_start = buf.len();
        let bad = ffi::UsnRecordV2Header {
            record_length: 4096,
            major_version: 2,
            minor_version: 0,
            file_reference_number: 3,
            parent_file_reference_number: 2,
            usn: 0,
            time_stamp: 0,
            reason: ffi::USN_REASON_FILE_CREATE,
            source_info: 0,
            security_id: 0,
            file_attributes: 0,
            file_name_length: 8,
            file_name_offset: HEADER_SIZE as u16,
        };
        let bad_bytes = unsafe {
            std::slice::from_raw_parts(
                (&bad as *const ffi::UsnRecordV2Header) as *const u8,
                HEADER_SIZE,
            )
        };
        buf.extend_from_slice(bad_bytes);
        // Pad so the buffer is real allocated memory beyond bytes_returned,
        // simulating stale bytes left over from a previous ioctl call.
        buf.extend_from_slice(&[0xAAu8; 64]);
        let bytes_returned = bad_start + HEADER_SIZE; // truncated: no room for the name

        let mut calls = 0;
        for_each_usn_record(&buf, bytes_returned, |_h, _name| {
            calls += 1;
        });
        assert_eq!(
            calls, 1,
            "only the first, well-formed record should be visited"
        );
    }

    #[test]
    fn zero_or_undersized_record_length_terminates() {
        let mut buf = vec![0u8; 8];
        let bad = ffi::UsnRecordV2Header {
            record_length: 4,
            major_version: 2,
            minor_version: 0,
            file_reference_number: 1,
            parent_file_reference_number: 2,
            usn: 0,
            time_stamp: 0,
            reason: ffi::USN_REASON_FILE_CREATE,
            source_info: 0,
            security_id: 0,
            file_attributes: 0,
            file_name_length: 0,
            file_name_offset: 0,
        };
        let bad_bytes = unsafe {
            std::slice::from_raw_parts(
                (&bad as *const ffi::UsnRecordV2Header) as *const u8,
                HEADER_SIZE,
            )
        };
        buf.extend_from_slice(bad_bytes);
        buf.extend_from_slice(&[0u8; 256]);
        let bytes_returned = buf.len();

        let mut calls = 0;
        for_each_usn_record(&buf, bytes_returned, |_h, _name| {
            calls += 1;
        });
        assert_eq!(
            calls, 0,
            "an undersized record_length must not spin forever"
        );
    }

    #[test]
    fn decode_name_into_handles_unpaired_surrogates() {
        // NTFS names are UCS-2 and may contain an unpaired surrogate, which
        // is not valid UTF-16. Lossy decoding must not panic.
        let mut valid: Vec<u16> = "readme".encode_utf16().collect();
        valid.push(0xD800); // unpaired high surrogate, no low surrogate follows
        let bytes: Vec<u8> = valid.iter().flat_map(|u| u.to_le_bytes()).collect();

        let mut out = String::new();
        decode_name_into(&bytes, &mut out);
        assert!(out.starts_with("readme"));
        assert!(
            out.contains('\u{FFFD}'),
            "unpaired surrogate should become U+FFFD"
        );
    }
}
