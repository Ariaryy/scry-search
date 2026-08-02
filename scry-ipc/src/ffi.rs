//! Hand-rolled kernel32 FFI for Windows named pipes, matching the style used
//! in scry-fsevents: exact control over layout/signatures rather than
//! depending on a generated binding's feature surface.

use std::ffi::c_void;

pub type Handle = *mut c_void;
pub type Bool = i32;
pub type Dword = u32;

pub const GENERIC_READ: Dword = 0x8000_0000;
pub const GENERIC_WRITE: Dword = 0x4000_0000;
pub const OPEN_EXISTING: Dword = 3;
pub const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

pub const PIPE_ACCESS_DUPLEX: Dword = 0x0000_0003;
pub const PIPE_TYPE_BYTE: Dword = 0x0000_0000;
pub const PIPE_READMODE_BYTE: Dword = 0x0000_0000;
pub const PIPE_WAIT: Dword = 0x0000_0000;
pub const PIPE_UNLIMITED_INSTANCES: Dword = 255;

pub const ERROR_PIPE_CONNECTED: Dword = 535;
pub const ERROR_PIPE_BUSY: Dword = 231;
pub const PAGE_READWRITE: Dword = 0x04;
pub const PAGE_READONLY: Dword = 0x02;
pub const FILE_MAP_READ: Dword = 0x0004;
pub const FILE_MAP_WRITE: Dword = 0x0002;
pub const SECTION_QUERY: Dword = 0x0001;
pub const PROCESS_DUP_HANDLE: Dword = 0x0040;

#[link(name = "kernel32")]
extern "system" {
    pub fn CreateNamedPipeW(
        lp_name: *const u16,
        dw_open_mode: Dword,
        dw_pipe_mode: Dword,
        n_max_instances: Dword,
        n_out_buffer_size: Dword,
        n_in_buffer_size: Dword,
        n_default_time_out: Dword,
        lp_security_attributes: *mut c_void,
    ) -> Handle;

    pub fn ConnectNamedPipe(h_named_pipe: Handle, lp_overlapped: *mut c_void) -> Bool;

    pub fn CreateFileW(
        lp_file_name: *const u16,
        dw_desired_access: Dword,
        dw_share_mode: Dword,
        lp_security_attributes: *mut c_void,
        dw_creation_disposition: Dword,
        dw_flags_and_attributes: Dword,
        h_template_file: Handle,
    ) -> Handle;

    pub fn ReadFile(
        h_file: Handle,
        lp_buffer: *mut c_void,
        n_number_of_bytes_to_read: Dword,
        lp_number_of_bytes_read: *mut Dword,
        lp_overlapped: *mut c_void,
    ) -> Bool;

    pub fn WriteFile(
        h_file: Handle,
        lp_buffer: *const c_void,
        n_number_of_bytes_to_write: Dword,
        lp_number_of_bytes_written: *mut Dword,
        lp_overlapped: *mut c_void,
    ) -> Bool;

    pub fn CloseHandle(h_object: Handle) -> Bool;

    pub fn WaitNamedPipeW(lp_named_pipe_name: *const u16, n_time_out: Dword) -> Bool;
    pub fn CreateFileMappingW(
        h_file: Handle,
        attributes: *mut c_void,
        protect: Dword,
        maximum_size_high: Dword,
        maximum_size_low: Dword,
        name: *const u16,
    ) -> Handle;
    pub fn MapViewOfFile(
        mapping: Handle,
        desired_access: Dword,
        offset_high: Dword,
        offset_low: Dword,
        bytes: usize,
    ) -> *mut c_void;
    pub fn UnmapViewOfFile(base: *const c_void) -> Bool;
    pub fn OpenProcess(desired_access: Dword, inherit: Bool, process_id: Dword) -> Handle;
    pub fn GetCurrentProcess() -> Handle;
    pub fn DuplicateHandle(
        source_process: Handle,
        source: Handle,
        target_process: Handle,
        target: *mut Handle,
        desired_access: Dword,
        inherit: Bool,
        options: Dword,
    ) -> Bool;
    pub fn GetNamedPipeClientProcessId(pipe: Handle, client_process_id: *mut Dword) -> Bool;
    #[cfg(test)]
    pub fn GetProcessHandleCount(process: Handle, handle_count: *mut Dword) -> Bool;

    pub fn LocalFree(h_mem: Handle) -> Handle;

    pub fn GetLastError() -> Dword;
}

#[repr(C)]
pub struct SecurityAttributes {
    pub n_length: Dword,
    pub lp_security_descriptor: *mut c_void,
    pub b_inherit_handle: Bool,
}

#[link(name = "advapi32")]
extern "system" {
    /// Parses an SDDL string into a security descriptor. Used so the pipe's
    /// DACL can be set explicitly at creation instead of inheriting the
    /// default, which — when scryd runs elevated — would leave the pipe
    /// reachable only by other elevated processes.
    pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: Dword,
        security_descriptor: *mut *mut c_void,
        security_descriptor_size: *mut Dword,
    ) -> Bool;
}
