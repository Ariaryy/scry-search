//! Platform filesystem-event backends. Windows-first (MFT bulk read + USN
//! Journal live updates); other platforms land behind the same trait later.

#[cfg(windows)]
pub mod mft;

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub use windows::{
    is_structural_reason, ChangeEvent, JournalHandle, WindowsBackend, WindowsBackendError,
};
