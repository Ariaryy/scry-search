//! Hand-rolled Win32 FFI for process/thread QoS configuration. Same rationale
//! as the other `ffi` modules in this workspace: a handful of calls doesn't
//! justify a binding crate, and the structs here are small and stable.

use std::ffi::c_void;

pub type Handle = *mut c_void;
pub type Bool = i32;
pub type Dword = u32;

/// PROCESS_INFORMATION_CLASS::ProcessMemoryPriority
pub const PROCESS_MEMORY_PRIORITY: i32 = 0;
/// PROCESS_INFORMATION_CLASS::ProcessPowerThrottling
pub const PROCESS_POWER_THROTTLING: i32 = 4;
/// THREAD_INFORMATION_CLASS::ThreadPowerThrottling
pub const THREAD_POWER_THROTTLING: i32 = 3;

pub const PROCESS_POWER_THROTTLING_CURRENT_VERSION: Dword = 1;
pub const THREAD_POWER_THROTTLING_CURRENT_VERSION: Dword = 1;
/// Opt the process/thread into EcoQoS: the scheduler prefers efficiency cores
/// and lower frequencies. Setting the bit in ControlMask *and* StateMask
/// enables it; ControlMask set with StateMask clear explicitly disables it;
/// both clear returns the target to system-managed default.
pub const POWER_THROTTLING_EXECUTION_SPEED: Dword = 0x0000_0001;

pub const MEMORY_PRIORITY_LOW: Dword = 2;
pub const THREAD_MODE_BACKGROUND_BEGIN: i32 = 0x0001_0000;
pub const THREAD_MODE_BACKGROUND_END: i32 = 0x0002_0000;

#[repr(C)]
pub struct ProcessPowerThrottlingState {
    pub version: Dword,
    pub control_mask: Dword,
    pub state_mask: Dword,
}

#[repr(C)]
pub struct ThreadPowerThrottlingState {
    pub version: Dword,
    pub control_mask: Dword,
    pub state_mask: Dword,
}

#[repr(C)]
pub struct MemoryPriorityInformation {
    pub memory_priority: Dword,
}

#[link(name = "kernel32")]
extern "system" {
    pub fn GetCurrentProcess() -> Handle;
    pub fn GetCurrentThread() -> Handle;

    pub fn SetProcessInformation(
        h_process: Handle,
        process_information_class: i32,
        process_information: *mut c_void,
        process_information_size: Dword,
    ) -> Bool;

    pub fn SetThreadInformation(
        h_thread: Handle,
        thread_information_class: i32,
        thread_information: *mut c_void,
        thread_information_size: Dword,
    ) -> Bool;

    pub fn GetLastError() -> Dword;
    pub fn SetThreadPriority(h_thread: Handle, priority: i32) -> Bool;
    pub fn SetProcessWorkingSetSizeEx(
        h_process: Handle,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        flags: Dword,
    ) -> Bool;
}

extern "C" {
    pub fn mi_collect(force: bool);
}
