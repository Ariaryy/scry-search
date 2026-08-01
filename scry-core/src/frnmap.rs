//! Cold FRN-to-record-index sidecar used only while applying structural events.

use memmap2::Mmap;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrnEntry {
    pub frn: u64,
    pub index: u32,
    pub _pad: u32,
}

pub struct FrnMap {
    mmap: Mmap,
}

impl FrnMap {
    pub fn save(path: &Path, entries: &mut [FrnEntry]) -> io::Result<()> {
        entries.sort_unstable_by_key(|entry| entry.frn);
        let tmp = path.with_extension("frn.tmp");
        {
            let mut file = File::create(&tmp)?;
            // SAFETY: FrnEntry is repr(C), contains no padding with unspecified
            // contents, and the slice remains alive for the duration of write_all.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    entries.as_ptr().cast::<u8>(),
                    std::mem::size_of_val(entries),
                )
            };
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(tmp, path)
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() % std::mem::size_of::<FrnEntry>() != 0
            || mmap.as_ptr().align_offset(std::mem::align_of::<FrnEntry>()) != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid FRN sidecar layout",
            ));
        }
        Ok(Self { mmap })
    }

    fn entries(&self) -> &[FrnEntry] {
        // SAFETY: open validated length and alignment; FrnEntry accepts every
        // bit pattern and the mapping is immutable for this object's lifetime.
        unsafe {
            std::slice::from_raw_parts(
                self.mmap.as_ptr().cast::<FrnEntry>(),
                self.mmap.len() / std::mem::size_of::<FrnEntry>(),
            )
        }
    }

    pub fn lookup(&self, frn: u64) -> Option<u32> {
        self.entries()
            .binary_search_by_key(&frn, |entry| entry.frn)
            .ok()
            .map(|position| self.entries()[position].index)
    }

    pub fn len(&self) -> usize {
        self.entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frn_entry_is_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<FrnEntry>(), 16);
    }

    #[test]
    fn frnmap_roundtrip_and_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.frn");
        let mut entries: Vec<_> = (0..10_000u32)
            .map(|index| FrnEntry {
                frn: (index as u64).wrapping_mul(7_919).wrapping_add(17),
                index,
                _pad: 0,
            })
            .collect();
        FrnMap::save(&path, &mut entries).unwrap();
        let map = FrnMap::open(&path).unwrap();
        assert_eq!(map.len(), 10_000);
        for entry in &entries {
            assert_eq!(map.lookup(entry.frn), Some(entry.index));
        }
        for absent in 0..1_000u64 {
            assert_eq!(map.lookup(absent * 7_919 + 18), None);
        }
    }

    #[test]
    fn frnmap_rejects_truncated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.frn");
        std::fs::write(&path, [0u8; 17]).unwrap();
        assert!(FrnMap::open(&path).is_err());
    }
}
