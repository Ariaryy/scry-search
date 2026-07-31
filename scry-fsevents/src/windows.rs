//! Windows backend: bulk-load the index via `FSCTL_ENUM_USN_DATA`, which walks
//! the NTFS MFT in on-disk (record) order rather than the directory tree —
//! turning "enumerate a million files" from a million small directory-read
//! syscalls into a handful of large sequential reads of the MFT itself.
//!
//! Live updates ride the same USN journal via `FSCTL_READ_USN_JOURNAL`,
//! polling with a bounded wait so the watcher thread can also notice a stop
//! request promptly.

use scry_core::{ArenaBuilder, EntryFlags, FileRecord};
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
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
    },
    Deleted {
        frn: u64,
    },
    /// Covers both a rename and a move (new parent), since NTFS reports both
    /// as a name-change record on the same FRN.
    Renamed {
        frn: u64,
        parent_frn: u64,
        name: String,
    },
    /// Metadata/content changed but identity, name and parent did not — the
    /// arena's tree shape is unaffected, so the daemon can ignore or use this
    /// only to refresh mtime.
    Modified {
        frn: u64,
    },
}

pub struct WindowsBackend;

impl WindowsBackend {
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

fn open_volume(volume: &str) -> Result<ffi::Handle, WindowsBackendError> {
    let path = format!("\\\\.\\{volume}");
    let wide = to_wide(&path);

    let handle = unsafe {
        ffi::CreateFileW(
            wide.as_ptr(),
            ffi::GENERIC_READ,
            ffi::FILE_SHARE_READ | ffi::FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            ffi::OPEN_EXISTING,
            0,
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

fn enumerate_mft(volume: &str) -> Result<Vec<RawEntry>, WindowsBackendError> {
    let handle = open_volume(volume)?;

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

fn query_journal(handle: ffi::Handle) -> Result<ffi::UsnJournalDataV0, WindowsBackendError> {
    let mut out = ffi::UsnJournalDataV0::default();
    let mut bytes_returned: u32 = 0;
    let ok = unsafe {
        ffi::DeviceIoControl(
            handle,
            ffi::FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut out as *mut _ as *mut c_void,
            std::mem::size_of::<ffi::UsnJournalDataV0>() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let code = unsafe { ffi::GetLastError() };
        return Err(WindowsBackendError::QueryJournal { code });
    }
    Ok(out)
}

/// Blocks the calling thread, translating USN journal change records into
/// [`ChangeEvent`]s on `tx` until `should_stop` is set or `tx`'s receiver is
/// dropped. Starts from "now" (the journal's current USN) — it does not
/// replay history, so callers must complete a bulk index first.
fn watch(
    volume: &str,
    tx: &crossbeam::channel::Sender<ChangeEvent>,
    should_stop: &AtomicBool,
) -> Result<(), WindowsBackendError> {
    let handle = open_volume(volume)?;
    let result = (|| {
        let journal = query_journal(handle)?;
        let mut start_usn = journal.next_usn;
        let mut out_buf = vec![0u8; 64 * 1024];

        while !should_stop.load(Ordering::Relaxed) {
            let input = ffi::ReadUsnJournalDataV0 {
                start_usn,
                reason_mask: 0xFFFF_FFFF,
                return_only_on_close: 0,
                // Bounded wait: lets the loop re-check `should_stop` on an
                // otherwise-idle volume instead of blocking indefinitely.
                timeout: 1,
                bytes_to_wait_for: 1,
                usn_journal_id: journal.usn_journal_id,
            };
            let mut bytes_returned: u32 = 0;

            let ok = unsafe {
                ffi::DeviceIoControl(
                    handle,
                    ffi::FSCTL_READ_USN_JOURNAL,
                    &input as *const _ as *const c_void,
                    std::mem::size_of::<ffi::ReadUsnJournalDataV0>() as u32,
                    out_buf.as_mut_ptr() as *mut c_void,
                    out_buf.len() as u32,
                    &mut bytes_returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let code = unsafe { ffi::GetLastError() };
                return Err(WindowsBackendError::ReadJournal { code });
            }
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
    result
}

fn classify(header: &ffi::UsnRecordV2Header, name: String) -> ChangeEvent {
    const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
    const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
    const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;

    let reason = header.reason;
    if reason & USN_REASON_FILE_DELETE != 0 {
        ChangeEvent::Deleted {
            frn: header.file_reference_number,
        }
    } else if reason & USN_REASON_FILE_CREATE != 0 {
        ChangeEvent::Created {
            frn: header.file_reference_number,
            parent_frn: header.parent_file_reference_number,
            name,
            is_dir: header.file_attributes & ffi::FILE_ATTRIBUTE_DIRECTORY != 0,
        }
    } else if reason & USN_REASON_RENAME_NEW_NAME != 0 {
        ChangeEvent::Renamed {
            frn: header.file_reference_number,
            parent_frn: header.parent_file_reference_number,
            name,
        }
    } else {
        ChangeEvent::Modified {
            frn: header.file_reference_number,
        }
    }
}

/// Handle to a background journal-watcher thread. Dropping this without
/// calling `stop` leaves the thread running detached — call `stop` to shut
/// it down deterministically (e.g. on daemon exit).
pub struct JournalHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<Result<(), WindowsBackendError>>>,
}

impl JournalHandle {
    pub fn stop(mut self) -> Result<(), WindowsBackendError> {
        self.stop.store(true, Ordering::Relaxed);
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
    pub fn spawn_watcher(volume: &str, tx: crossbeam::channel::Sender<ChangeEvent>) -> JournalHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let volume = volume.to_string();
        let join = std::thread::spawn(move || watch(&volume, &tx, &stop_thread));
        JournalHandle {
            stop,
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

    pub const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

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
    }
}
