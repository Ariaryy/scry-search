use rkyv::{Archive, Deserialize, Serialize};

/// One indexed filesystem entry. Kept small and flat — this is the unit
/// that gets multiplied by millions, so every extra byte here is millions
/// of bytes of daemon-visible working set.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
#[archive(check_bytes)]
pub struct FileRecord {
    /// Index into the arena's record vec, u32::MAX for "no parent" (volume root).
    pub parent: u32,
    /// Just the leaf name — full paths are reconstructed by walking `parent`.
    pub name: String,
    pub size: u64,
    /// Windows FILETIME (100ns ticks since 1601-01-01), matches what NTFS gives us
    /// natively so no conversion happens on the hot ingest path.
    pub mtime: i64,
    pub flags: EntryFlags,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum EntryFlags {
    File = 0,
    Directory = 1,
}

impl FileRecord {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags == EntryFlags::Directory
    }
}
