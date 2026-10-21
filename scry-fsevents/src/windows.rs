//! Windows backend: bulk-load the index via `FSCTL_ENUM_USN_DATA`, which walks
//! the NTFS MFT in on-disk (record) order rather than the directory tree —
//! turning "enumerate a million files" from a million small directory-read
//! syscalls into a handful of large sequential reads of the MFT itself.
//!
//! Live updates ride the same USN journal via `FSCTL_READ_USN_JOURNAL`, using
//! overlapped I/O so the watcher thread blocks at 0% CPU when the volume is
//! idle; `JournalHandle::stop` unblocks it deterministically via `CancelIoEx`.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use thiserror::Error;
mod ffi;
mod indexing;
mod journal;
mod records;

pub use indexing::enumerate_mft_usn;
use indexing::{build_raw_index, build_usn_index};
pub use journal::{is_structural_reason, JournalHandle};
use journal::{journal_cursor, replay_journal};

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
    #[error("GetVolumeInformationW failed: win32 error {code}")]
    VolumeInformation { code: u32 },
    #[error("USN journal identity changed")]
    JournalChanged,
    #[error("requested USN cursor has aged out of the journal")]
    JournalAgedOut,
}

/// Identity and replay position for an NTFS USN journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalCursor {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub volume_serial: u64,
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
    /// Confirms every prior event in this read batch has been delivered.
    Advanced {
        next_usn: i64,
    },
}

impl ChangeEvent {
    pub fn is_auxiliary(&self) -> bool {
        match self {
            ChangeEvent::Created { is_auxiliary, .. }
            | ChangeEvent::Deleted { is_auxiliary, .. }
            | ChangeEvent::Renamed { is_auxiliary, .. }
            | ChangeEvent::Modified { is_auxiliary, .. } => *is_auxiliary,
            ChangeEvent::Advanced { .. } => false,
        }
    }
}

pub struct WindowsBackend;

impl WindowsBackend {
    /// Captures the active journal identity before an index enumeration.
    pub fn journal_cursor(volume: &str) -> Result<JournalCursor, WindowsBackendError> {
        journal_cursor(volume)
    }
    /// Reads structural journal records from `cursor` through the current
    /// cursor. The returned cursor is safe to persist after all events apply.
    pub fn replay_journal(
        volume: &str,
        cursor: JournalCursor,
    ) -> Result<(JournalCursor, Vec<ChangeEvent>), WindowsBackendError> {
        replay_journal(volume, cursor)
    }
    /// Enumerates accessible fixed NTFS drive roots. Failed probes are omitted;
    /// callers can continue indexing the remaining volumes.
    pub fn fixed_ntfs_volumes() -> Vec<String> {
        let drives = unsafe { ffi::GetLogicalDrives() };
        let mut volumes = Vec::new();
        for bit in 0..26 {
            if drives & (1 << bit) == 0 {
                continue;
            }
            let letter = char::from(b'A' + bit as u8);
            let root = format!("{letter}:\\");
            let wide = to_wide(&root);
            if unsafe { ffi::GetDriveTypeW(wide.as_ptr()) } != ffi::DRIVE_FIXED {
                continue;
            }
            let mut filesystem = [0u16; 32];
            let ok = unsafe {
                ffi::GetVolumeInformationW(
                    wide.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    filesystem.as_mut_ptr(),
                    filesystem.len() as u32,
                )
            };
            if ok == 0 {
                continue;
            }
            let length = filesystem
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(filesystem.len());
            if String::from_utf16_lossy(&filesystem[..length]).eq_ignore_ascii_case("NTFS") {
                volumes.push(format!("{letter}:"));
            }
        }
        volumes
    }

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
        // Which reader served which volume, and why, is worth a line each:
        // the two differ in whether `size` carries any information at all, and
        // a volume silently demoting to USN is otherwise indistinguishable
        // from a volume where every file happens to be empty.
        match build_raw_index(volume) {
            Ok((arena, frns, report)) => {
                let unknown = report.files_with_unknown_size;
                let empty = report.files_with_confirmed_empty_size;
                eprintln!(
                    "scry: {volume} indexed by the raw $MFT reader: {} entries, \
                     {unknown} of them files with unknown size ({:.2}%), \
                     {empty} confirmed genuinely empty ({:.2}%), \
                     {} unresolved attribute lists",
                    report.emitted,
                    100.0 * unknown as f64 / report.emitted.max(1) as f64,
                    100.0 * empty as f64 / report.emitted.max(1) as f64,
                    report.attribute_list_unresolved,
                );
                eprintln!("scry: {volume} raw reader detail: {report:?}");
                Ok((arena, frns))
            }
            Err(error) => {
                eprintln!(
                    "scry: {volume} demoted to USN enumeration ({error}); \
                     size is unavailable from USN records, so every entry on \
                     this volume will report 0"
                );
                build_usn_index(volume)
            }
        }
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

fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests;
