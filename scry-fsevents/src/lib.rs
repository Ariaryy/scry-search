//! Platform filesystem-event backends.
//! Windows-first (MFT bulk read + USN Journal live updates); other platforms
//! land behind the same trait later.

#[cfg(windows)]
pub mod mft;
#[cfg(windows)]
mod throttle;
#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub fn configure_index_read_cap(bytes_per_second: u64) {
    throttle::configure(bytes_per_second);
}

#[cfg(windows)]
pub use windows::{
    enumerate_mft_usn, is_structural_reason, ChangeEvent, JournalCursor, JournalHandle,
    WindowsBackend, WindowsBackendError,
};
