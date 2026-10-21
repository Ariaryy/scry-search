//! Windows background scheduling and resource policy.

use crate::ffi;

pub(super) fn configure_index_read_cap(cli_value: Option<u64>) {
    const MIB: u64 = 1024 * 1024;
    let mebibytes_per_second =
        cli_value.unwrap_or_else(|| match std::env::var("SCRY_INDEX_MBPS") {
            Ok(value) => match value.parse::<u64>() {
                Ok(value) => value,
                Err(_) => {
                    eprintln!("scryd: ignoring invalid SCRY_INDEX_MBPS={value:?}; using 128");
                    128
                }
            },
            Err(_) => 128,
        });
    scry_fsevents::configure_index_read_cap(mebibytes_per_second.saturating_mul(MIB));
    eprintln!("scryd: index read cap {mebibytes_per_second} MiB/s");
}

/// Configure this process as a well-behaved background service:
///
/// - **EcoQoS** (`ProcessPowerThrottling` / `EXECUTION_SPEED`): tells the
///   Windows scheduler to prefer efficiency cores and lower clocks. A file
///   indexer has no latency requirement that justifies boost clocks, and this
///   is the single largest lever on the daemon's power draw.
/// - **Low memory priority**: the index is a cache that can be re-faulted from
///   the snapshot file, so under memory pressure these pages should be
///   reclaimed before a foreground app's.
///
/// Both are best-effort. Failures are logged and ignored — an older Windows
/// build simply doesn't support them, and that is not a reason to refuse to
/// run.
pub(super) fn configure_background_qos() {
    use std::mem::size_of;

    // SAFETY: all pointers point to local structs with the correct layout;
    // the Win32 functions are documented to read only within the supplied size.
    unsafe {
        // EcoQoS: prefer efficiency cores / lower frequencies.
        let mut throttle = ffi::ProcessPowerThrottlingState {
            version: ffi::PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            control_mask: ffi::POWER_THROTTLING_EXECUTION_SPEED,
            state_mask: ffi::POWER_THROTTLING_EXECUTION_SPEED,
        };
        let result = ffi::SetProcessInformation(
            ffi::GetCurrentProcess(),
            ffi::PROCESS_POWER_THROTTLING,
            &mut throttle as *mut _ as *mut std::ffi::c_void,
            size_of::<ffi::ProcessPowerThrottlingState>() as ffi::Dword,
        );
        if result == 0 {
            let code = ffi::GetLastError();
            eprintln!("scryd: EcoQoS unavailable (win32 error {code}); continuing at default QoS");
        }

        // Low memory priority: reclaim daemon pages first under pressure.
        let mut mem_prio = ffi::MemoryPriorityInformation {
            memory_priority: ffi::MEMORY_PRIORITY_LOW,
        };
        let result = ffi::SetProcessInformation(
            ffi::GetCurrentProcess(),
            ffi::PROCESS_MEMORY_PRIORITY,
            &mut mem_prio as *mut _ as *mut std::ffi::c_void,
            size_of::<ffi::MemoryPriorityInformation>() as ffi::Dword,
        );
        if result == 0 {
            let code = ffi::GetLastError();
            eprintln!("scryd: low memory priority unavailable (win32 error {code}); continuing");
        }
    }
}

/// Apply EcoQoS to the calling thread. Must be called from the thread being
/// throttled (self-application avoids handle lifetime issues). Only apply to
/// the reindex worker — query-serving threads must stay responsive.
pub(super) fn configure_background_thread_qos() {
    use std::mem::size_of;

    // SAFETY: same as configure_background_qos.
    unsafe {
        let mut throttle = ffi::ThreadPowerThrottlingState {
            version: ffi::THREAD_POWER_THROTTLING_CURRENT_VERSION,
            control_mask: ffi::POWER_THROTTLING_EXECUTION_SPEED,
            state_mask: ffi::POWER_THROTTLING_EXECUTION_SPEED,
        };
        let result = ffi::SetThreadInformation(
            ffi::GetCurrentThread(),
            ffi::THREAD_POWER_THROTTLING,
            &mut throttle as *mut _ as *mut std::ffi::c_void,
            size_of::<ffi::ThreadPowerThrottlingState>() as ffi::Dword,
        );
        if result == 0 {
            let code = ffi::GetLastError();
            eprintln!(
                "scryd: thread EcoQoS unavailable (win32 error {code}); reindex thread at default QoS"
            );
        }
    }
}

pub(super) struct BackgroundModeGuard {
    entered: bool,
}

impl BackgroundModeGuard {
    pub(super) fn enter() -> Self {
        let entered = unsafe {
            ffi::SetThreadPriority(ffi::GetCurrentThread(), ffi::THREAD_MODE_BACKGROUND_BEGIN) != 0
        };
        if !entered {
            eprintln!(
                "scryd: thread background mode unavailable (win32 error {}); continuing",
                unsafe { ffi::GetLastError() }
            );
        }
        Self { entered }
    }
}

impl Drop for BackgroundModeGuard {
    fn drop(&mut self) {
        if self.entered {
            unsafe {
                ffi::SetThreadPriority(ffi::GetCurrentThread(), ffi::THREAD_MODE_BACKGROUND_END);
            }
        }
    }
}

pub(super) fn trim_working_set() {
    unsafe {
        ffi::mi_collect(true);
        ffi::SetProcessWorkingSetSizeEx(ffi::GetCurrentProcess(), usize::MAX, usize::MAX, 0);
    }
}
