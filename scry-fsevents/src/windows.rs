//! Windows backend: bulk-load the index via `FSCTL_ENUM_USN_DATA`, which walks
//! the NTFS MFT in on-disk (record) order rather than the directory tree —
//! turning "enumerate a million files" from a million small directory-read
//! syscalls into a handful of large sequential reads of the MFT itself.
//!
//! Live updates ride the same USN journal via `FSCTL_READ_USN_JOURNAL`, using
//! overlapped I/O so the watcher thread blocks at 0% CPU when the volume is
//! idle; `JournalHandle::stop` unblocks it deterministically via `CancelIoEx`.

use scry_core::{ArenaBuilder, EntryFlags, FileRecord};
use std::collections::HashMap;
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
    pub fn mark_handle_as_auxiliary(
        file: &std::fs::File,
        volume: &str,
    ) -> Result<(), ffi::Dword> {
        mark_handle_as_auxiliary(file, volume)
    }

    /// Bulk-enumerate a volume (e.g. `"C:"`) directly from its MFT via the USN
    /// journal's enumeration ioctl, bypassing per-file stat()/directory walks.
    /// Requires the process to hold `SeBackupPrivilege` (i.e. run elevated).
    pub fn bulk_index_volume(volume: &str) -> Result<scry_core::Arena, WindowsBackendError> {
        let entries = enumerate_mft(volume)?;

        let mut builder = ArenaBuilder::default();
        let mut frn_to_idx: HashMap<u64, u32> = HashMap::with_capacity(entries.len());

        for e in &entries {
            let idx = builder.push(FileRecord {
                parent: u32::MAX, // resolved in the second pass below
                name: e.name.clone(),
                size: 0, // USN records don't carry size; left for a lazy stat pass
                mtime: e.mtime,
                flags: if e.is_dir {
                    EntryFlags::Directory
                } else {
                    EntryFlags::File
                },
            });
            frn_to_idx.insert(e.frn, idx);
        }

        // The NTFS root record (self-parented, or simply never referenced by
        // any child's parent FRN, depending on what the enumeration yields)
        // otherwise contributes nothing to full_path() — every top-level
        // entry's parent chain would just stop, dropping the drive letter.
        // Give every "no real parent" entry an explicit volume-root node
        // instead of `u32::MAX` so paths always start with e.g. `C:`.
        let root_idx = builder.push(FileRecord {
            parent: u32::MAX,
            name: volume.to_string(),
            size: 0,
            mtime: 0,
            flags: EntryFlags::Directory,
        });

        for (i, e) in entries.iter().enumerate() {
            let idx = i as u32;
            match frn_to_idx.get(&e.parent_frn) {
                Some(&p) if p != idx => builder.set_parent(idx, p),
                _ => builder.set_parent(idx, root_idx),
            }
        }

        Ok(builder.build())
    }
}

struct RawEntry {
    frn: u64,
    parent_frn: u64,
    name: String,
    is_dir: bool,
    mtime: i64,
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
        let mut luid = ffi::Luid { low_part: 0, high_part: 0 };
        if ffi::LookupPrivilegeValueW(std::ptr::null(), wide_name.as_ptr(), &mut luid) == 0 {
            let err = ffi::GetLastError();
            ffi::CloseHandle(token);
            return Err(err);
        }

        let mut privileges = ffi::TokenPrivileges {
            privilege_count: 1,
            privileges: ffi::LuidAndAttributes {
                luid,
                attributes: ffi::SE_PRIVILEGE_ENABLED,
            },
        };
        let ok = ffi::AdjustTokenPrivileges(
            token,
            0,
            &mut privileges,
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
pub fn mark_handle_as_auxiliary(
    file: &std::fs::File,
    volume: &str,
) -> Result<(), ffi::Dword> {
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
        if ok != 0 { Ok(()) } else { Err(ffi::GetLastError()) }
    };

    unsafe { ffi::CloseHandle(volume_handle) };
    result
}

fn enumerate_mft(volume: &str) -> Result<Vec<RawEntry>, WindowsBackendError> {
    let handle = open_volume(volume, 0)?;

    let result = (|| {
        let mut entries = Vec::new();
        let mut start_frn: u64 = 0;
        // 64KiB output buffer: large enough to amortize the syscall over many
        // records per call, small enough to keep RSS low even if several
        // enumerations run concurrently.
        let mut out_buf = vec![0u8; 64 * 1024];

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

            for (header, name) in parse_usn_records(&out_buf, bytes_returned as usize) {
                entries.push(RawEntry {
                    frn: header.file_reference_number,
                    parent_frn: header.parent_file_reference_number,
                    name,
                    is_dir: header.file_attributes & ffi::FILE_ATTRIBUTE_DIRECTORY != 0,
                    mtime: header.time_stamp,
                });
            }

            if next_start == start_frn {
                break;
            }
            start_frn = next_start;
        }

        Ok(entries)
    })();

    unsafe { ffi::CloseHandle(handle) };
    result
}

/// Parses a `FSCTL_ENUM_USN_DATA`/`FSCTL_READ_USN_JOURNAL` output buffer into
/// its USN_RECORD_V2 entries. Both ioctls share this exact wire format (an
/// 8-byte cursor followed by a packed sequence of variable-length records),
/// so enumeration and live-journal reads reuse the same parser.
fn parse_usn_records(buf: &[u8], bytes_returned: usize) -> Vec<(ffi::UsnRecordV2Header, String)> {
    let mut out = Vec::new();
    let mut offset = 8usize;
    while offset + std::mem::size_of::<ffi::UsnRecordV2Header>() <= bytes_returned {
        let header: ffi::UsnRecordV2Header =
            unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr() as *const _) };
        if header.record_length == 0 {
            break;
        }

        let name_start = offset + header.file_name_offset as usize;
        let name_end = name_start + header.file_name_length as usize;
        let name = if name_end <= buf.len() {
            let name_bytes = &buf[name_start..name_end];
            let utf16: Vec<u16> = name_bytes
                .chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&utf16)
        } else {
            String::new()
        };

        out.push((header, name));
        offset += header.record_length as usize;
    }
    out
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
fn watch(
    volume: &str,
    tx: &crossbeam::channel::Sender<ChangeEvent>,
    should_stop: &AtomicBool,
    watch_handle: &AtomicUsize,
) -> Result<(), WindowsBackendError> {
    let handle = open_volume(volume, ffi::FILE_FLAG_OVERLAPPED)?;
    watch_handle.store(handle as usize, Ordering::Release);

    let result = (|| {
        let journal = query_journal(handle)?;
        let mut start_usn = journal.next_usn;
        let mut out_buf = vec![0u8; 64 * 1024];

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

            for (header, name) in parse_usn_records(&out_buf, bytes_returned as usize) {
                let event = classify(&header, name);
                if tx.send(event).is_err() {
                    return Ok(()); // receiver gone, nothing left to do
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
    join: Option<std::thread::JoinHandle<Result<(), WindowsBackendError>>>,
}

impl JournalHandle {
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
        let volume = volume.to_string();
        let join =
            std::thread::spawn(move || watch(&volume, &tx, &stop_thread, &watch_handle_thread));
        JournalHandle {
            stop,
            watch_handle,
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
