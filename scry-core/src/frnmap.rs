//! Cold FRN-to-record-index sidecar used only while applying structural events.

use memmap2::Mmap;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

const MAGIC: [u8; 8] = *b"SCRYFRN\0";
const SIDECAR_VERSION: u32 = 1;
const HEADER_LEN: usize = 16;

fn write_header(mut writer: impl Write, snapshot_tag: u32) -> io::Result<()> {
    writer.write_all(&MAGIC)?;
    writer.write_all(&SIDECAR_VERSION.to_le_bytes())?;
    writer.write_all(&snapshot_tag.to_le_bytes())
}

fn invalid_layout() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid or mismatched FRN sidecar",
    )
}

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
    pub fn save(path: &Path, entries: &mut [FrnEntry], snapshot_tag: u32) -> io::Result<()> {
        Self::save_with(path, entries, snapshot_tag, |_| {})
    }

    pub fn save_with<F>(
        path: &Path,
        entries: &mut [FrnEntry],
        snapshot_tag: u32,
        on_create: F,
    ) -> io::Result<()>
    where
        F: FnOnce(&File),
    {
        entries.sort_unstable_by_key(|entry| entry.frn);
        let tmp = path.with_extension("frn.tmp");
        {
            let mut file = File::create(&tmp)?;
            on_create(&file);
            write_header(&mut file, snapshot_tag)?;
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

    /// Writes a sidecar from an already frn-ascending stream, without ever
    /// collecting it into a slice to sort. Compaction's caller produces
    /// exactly such a stream by merging the base sidecar's entries (already
    /// frn-sorted on disk, filtered and remapped to final indices — a
    /// filtered subsequence of a sorted sequence stays sorted) with the
    /// small delta FRN list (sorted once, in memory, since it is bounded by
    /// the compaction threshold rather than base size). Writes go straight
    /// to a `BufWriter`, so this holds no more than one entry's worth of
    /// memory beyond the caller's iterator.
    ///
    /// # Panics (debug only)
    /// Panics if `entries` is not actually frn-ascending — this function
    /// trusts the caller's ordering rather than re-sorting, so a caller that
    /// violates it silently corrupts `lookup`'s binary search instead of
    /// failing loudly in release builds.
    pub fn save_streaming<I, F>(
        path: &Path,
        entries: I,
        snapshot_tag: u32,
        on_create: F,
    ) -> io::Result<()>
    where
        I: IntoIterator<Item = FrnEntry>,
        F: FnOnce(&File),
    {
        let tmp = path.with_extension("frn.tmp");
        {
            let file = File::create(&tmp)?;
            on_create(&file);
            let mut writer = io::BufWriter::new(file);
            write_header(&mut writer, snapshot_tag)?;
            #[cfg(debug_assertions)]
            let mut last_frn: Option<u64> = None;
            for entry in entries {
                #[cfg(debug_assertions)]
                {
                    if let Some(last) = last_frn {
                        debug_assert!(
                            last <= entry.frn,
                            "save_streaming requires frn-ascending input"
                        );
                    }
                    last_frn = Some(entry.frn);
                }
                // SAFETY: FrnEntry is repr(C) with no padding of unspecified
                // contents.
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        (&entry as *const FrnEntry).cast::<u8>(),
                        std::mem::size_of::<FrnEntry>(),
                    )
                };
                writer.write_all(bytes)?;
            }
            writer.flush()?;
            writer.into_inner()?.sync_all()?;
        }
        std::fs::rename(tmp, path)
    }

    pub fn open(path: &Path, expected_snapshot_tag: u32) -> io::Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        let header = mmap.get(..HEADER_LEN).ok_or_else(invalid_layout)?;
        if header[..8] != MAGIC
            || u32::from_le_bytes(header[8..12].try_into().expect("four bytes")) != SIDECAR_VERSION
            || u32::from_le_bytes(header[12..16].try_into().expect("four bytes"))
                != expected_snapshot_tag
            || !(mmap.len() - HEADER_LEN).is_multiple_of(std::mem::size_of::<FrnEntry>())
            || unsafe { mmap.as_ptr().add(HEADER_LEN) }
                .align_offset(std::mem::align_of::<FrnEntry>())
                != 0
        {
            return Err(invalid_layout());
        }
        Ok(Self { mmap })
    }

    fn entries(&self) -> &[FrnEntry] {
        // SAFETY: open validated length and alignment; FrnEntry accepts every
        // bit pattern and the mapping is immutable for this object's lifetime.
        unsafe {
            std::slice::from_raw_parts(
                self.mmap.as_ptr().add(HEADER_LEN).cast::<FrnEntry>(),
                (self.mmap.len() - HEADER_LEN) / std::mem::size_of::<FrnEntry>(),
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

    pub fn iter(&self) -> impl Iterator<Item = FrnEntry> + '_ {
        self.entries().iter().copied()
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
        FrnMap::save(&path, &mut entries, 17).unwrap();
        let map = FrnMap::open(&path, 17).unwrap();
        assert_eq!(map.len(), 10_000);
        for entry in &entries {
            assert_eq!(map.lookup(entry.frn), Some(entry.index));
        }
        for absent in 0..1_000u64 {
            assert_eq!(map.lookup(absent * 7_919 + 18), None);
        }
    }

    #[test]
    fn save_streaming_matches_save_on_the_same_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries: Vec<_> = (0..5_000u32)
            .map(|index| FrnEntry {
                frn: (index as u64).wrapping_mul(3_559).wrapping_add(11),
                index,
                _pad: 0,
            })
            .collect();

        let sorted_path = dir.path().join("sorted.frn");
        FrnMap::save(&sorted_path, &mut entries.clone(), 29).unwrap();

        entries.sort_unstable_by_key(|entry| entry.frn);
        let streamed_path = dir.path().join("streamed.frn");
        FrnMap::save_streaming(&streamed_path, entries.iter().copied(), 29, |_| {}).unwrap();

        assert_eq!(
            std::fs::read(&sorted_path).unwrap(),
            std::fs::read(&streamed_path).unwrap()
        );
    }

    #[test]
    fn frnmap_rejects_truncated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.frn");
        std::fs::write(&path, [0u8; 17]).unwrap();
        assert!(FrnMap::open(&path, 1).is_err());
    }

    #[test]
    fn frnmap_rejects_a_different_snapshot_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paired.frn");
        let mut entries = [FrnEntry {
            frn: 7,
            index: 0,
            _pad: 0,
        }];
        FrnMap::save(&path, &mut entries, 41).unwrap();

        assert!(FrnMap::open(&path, 42).is_err());
    }
}
