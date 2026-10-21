//! Minimal Win32 declarations for volume and journal I/O.

use std::ffi::c_void;

pub type Handle = *mut c_void;
pub type Bool = i32;
pub type Dword = u32;

pub const GENERIC_READ: Dword = 0x8000_0000;
pub const FILE_SHARE_READ: Dword = 0x0000_0001;
pub const FILE_SHARE_WRITE: Dword = 0x0000_0002;
pub const OPEN_EXISTING: Dword = 3;
pub const ERROR_HANDLE_EOF: Dword = 38;
pub const ERROR_INVALID_DATA: Dword = 13;
pub const DRIVE_FIXED: Dword = 3;
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
    pub fn GetLogicalDrives() -> Dword;
    pub fn GetDriveTypeW(root_path_name: *const u16) -> Dword;
    pub fn GetVolumeInformationW(
        root_path_name: *const u16,
        volume_name_buffer: *mut u16,
        volume_name_size: Dword,
        volume_serial_number: *mut Dword,
        maximum_component_length: *mut Dword,
        file_system_flags: *mut Dword,
        file_system_name_buffer: *mut u16,
        file_system_name_size: Dword,
    ) -> Bool;
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
