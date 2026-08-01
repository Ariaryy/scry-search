//! Bounds-checked raw NTFS metadata reader.

pub mod boot;
pub mod runlist;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MftError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid NTFS structure: {0}")]
    Invalid(&'static str),
}
