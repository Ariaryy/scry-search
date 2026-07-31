use crate::arena::{Arena, ArchivedArena};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("archive validation failed: {0}")]
    Validation(String),
}

/// Serialize an Arena to disk via rkyv. This is the only place allocation-heavy
/// serialization happens — it's an offline/background step (snapshot compaction),
/// never on the query path.
pub fn save(arena: &Arena, path: &Path) -> Result<(), StoreError> {
    let bytes = rkyv::to_bytes::<_, 1024>(arena)
        .map_err(|e| StoreError::Validation(e.to_string()))?;
    let tmp_path = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp_path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp_path, path)?;
    Ok(())
}

/// An mmap-backed, zero-copy view of a persisted Arena. Opening this does not
/// deserialize anything — the OS page cache backs the memory, and `archived()`
/// just casts bytes. This is why daemon warm-start is near-instant regardless
/// of index size, and why RSS stays low even with a multi-GB index.
pub struct ArenaStore {
    mmap: Mmap,
}

impl ArenaStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        // Validate once at open time (bytecheck), not on every query.
        rkyv::check_archived_root::<Arena>(&mmap[..])
            .map_err(|e| StoreError::Validation(e.to_string()))?;
        Ok(Self { mmap })
    }

    #[inline]
    pub fn archived(&self) -> &ArchivedArena {
        // Safety: validated in `open` via check_archived_root.
        unsafe { rkyv::archived_root::<Arena>(&self.mmap[..]) }
    }
}
