//! Windows backend: bulk-load the index via `FSCTL_ENUM_USN_DATA`, which walks
//! the NTFS MFT in on-disk (record) order rather than the directory tree. This
//! is the actual trick behind Everything's speed — it turns "enumerate a
//! million files" from a million small directory-read syscalls into a handful
//! of large sequential reads of the MFT itself.
//!
//! Live updates via the USN Journal's change-record stream are task #4.

use scry_core::{ArenaBuilder, EntryFlags, FileRecord};
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum WindowsBackendError {
    #[error("failed to open volume {volume}: win32 error {code}")]
    OpenVolume { volume: String, code: u32 },
    #[error("DeviceIoControl(FSCTL_ENUM_USN_DATA) failed: win32 error {code}")]
    Enumerate { code: u32 },
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

        for (i, e) in entries.iter().enumerate() {
            let idx = i as u32;
            match frn_to_idx.get(&e.parent_frn) {
                // NTFS's root record is its own parent — treat that as "no parent"
                // rather than let full_path() spin in a cycle.
                Some(&p) if p != idx => builder.set_parent(idx, p),
                _ => builder.set_parent(idx, u32::MAX),
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

fn enumerate_mft(volume: &str) -> Result<Vec<RawEntry>, WindowsBackendError> {
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

            let mut offset = 8usize;
            while offset + std::mem::size_of::<ffi::UsnRecordV2Header>() <= bytes_returned as usize
            {
                let header: ffi::UsnRecordV2Header = unsafe {
                    std::ptr::read_unaligned(out_buf[offset..].as_ptr() as *const _)
                };
                if header.record_length == 0 {
                    break;
                }

                let name_start = offset + header.file_name_offset as usize;
                let name_end = name_start + header.file_name_length as usize;
                let name = if name_end <= out_buf.len() {
                    let name_bytes = &out_buf[name_start..name_end];
                    let utf16: Vec<u16> = name_bytes
                        .chunks_exact(2)
                        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                        .collect();
                    String::from_utf16_lossy(&utf16)
                } else {
                    String::new()
                };

                entries.push(RawEntry {
                    frn: header.file_reference_number,
                    parent_frn: header.parent_file_reference_number,
                    name,
                    is_dir: header.file_attributes & ffi::FILE_ATTRIBUTE_DIRECTORY != 0,
                    mtime: header.time_stamp,
                });

                offset += header.record_length as usize;
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

    pub const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

    #[repr(C)]
    pub struct MftEnumDataV0 {
        pub start_file_reference_number: u64,
        pub low_usn: i64,
        pub high_usn: i64,
    }

    /// Fixed-size header of USN_RECORD_V2; the filename follows at
    /// `file_name_offset` bytes from the start of the record.
    #[repr(C)]
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
