//! Windows backend: bulk-load the index from the NTFS MFT, then track live
//! changes via the USN Journal. Stubbed for now — see task #3/#4.

use scry_core::ArenaBuilder;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum WindowsBackendError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),
}

pub struct WindowsBackend;

impl WindowsBackend {
    /// Bulk-enumerate a volume (e.g. `C:`) directly from its MFT, bypassing
    /// per-file stat() calls. Not yet implemented — placeholder wires the
    /// crate boundary so scry-daemon can depend on it today.
    pub fn bulk_index_volume(_volume: &str) -> Result<ArenaBuilder, WindowsBackendError> {
        Err(WindowsBackendError::Unimplemented("MFT bulk read"))
    }
}
