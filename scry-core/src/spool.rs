//! File-backed replacements for the `Vec<T>` scratch columns compaction used
//! to build before handing a finished `Arena` to the serializer. A mutable
//! mmap's dirty pages are written back to the backing file and counted by
//! Windows as "Mapped File" memory (`WorkingSetSize`), not `PrivateUsage` —
//! see the streaming-compaction note in AGENTS.md — so a spool holding a
//! million-record column costs the same handful of live pages as a much
//! smaller one, rather than a proportional heap allocation.
//!
//! No `bytemuck`/`zerocopy` dependency: `Pod` is hand-rolled, matching the
//! rest of the crate's preference for minimal, explicit unsafe over a crate
//! pulled in for a handful of impls.

use memmap2::MmapMut;
use std::fs::{File, OpenOptions};
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

/// Marker for types safe to store in a [`Spool`] or [`ByteSpool`] by raw byte
/// copy: no padding, and every bit pattern of `size_of::<T>()` bytes is a
/// valid `T`.
///
/// # Safety
/// Implementors must have no padding bytes and must be valid for any bit
/// pattern of their size.
pub unsafe trait Pod: Copy + 'static {}

unsafe impl Pod for u8 {}
unsafe impl Pod for u32 {}
unsafe impl Pod for u64 {}
unsafe impl Pod for crate::frnmap::FrnEntry {}

/// A fixed-capacity, file-backed `Vec<T>` substitute. Compaction always knows
/// the exact final record count (`live_base + live_delta`) before its merge
/// pass starts, so every column except the front-coded name blob can reserve
/// its file up front and grow only its logical `len`, never its allocation.
///
/// The backing file is deleted when the spool is dropped — it is scratch,
/// never a durable artifact — so a spool must outlive every borrow taken from
/// [`Spool::as_slice`].
pub struct Spool<T: Pod> {
    file: File,
    mmap: MmapMut,
    len: usize,
    capacity: usize,
    path: PathBuf,
    _marker: PhantomData<T>,
}

impl<T: Pod> Spool<T> {
    /// Creates the backing file at `path`, truncating any existing file, and
    /// reserves room for exactly `capacity` elements. `on_create` runs against
    /// the freshly created handle before any bytes are mapped, so callers can
    /// mark it auxiliary (`FSCTL_MARK_HANDLE`) before it can generate a USN
    /// journal entry a watcher would see.
    pub fn create(path: &Path, capacity: usize, on_create: impl FnOnce(&File)) -> io::Result<Self> {
        let elem_len = std::mem::size_of::<T>();
        let byte_len = (capacity.max(1) * elem_len) as u64;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        on_create(&file);
        file.set_len(byte_len)?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        Ok(Spool {
            file,
            mmap,
            len: 0,
            capacity,
            path: path.to_path_buf(),
            _marker: PhantomData,
        })
    }

    /// Like [`Spool::create`], but immediately reports the full `capacity` as
    /// written. Valid because `set_len` on a freshly created file zero-fills
    /// it, matching `vec![T::default_bit_pattern; capacity]` semantics for
    /// any `T` whose all-zero pattern is meaningful (every `Pod` type used in
    /// this crate). Used for columns compaction fills by index instead of by
    /// append — a visited bitset, or `positions`/`subtree_ends`, which the
    /// DFS walk writes at arbitrary record indices rather than in order.
    pub fn zeroed(path: &Path, capacity: usize, on_create: impl FnOnce(&File)) -> io::Result<Self> {
        let mut spool = Self::create(path, capacity, on_create)?;
        spool.len = capacity;
        Ok(spool)
    }

    #[inline]
    pub fn push(&mut self, value: T) {
        assert!(self.len < self.capacity, "spool capacity exceeded");
        self.write_at(self.len, value);
        self.len += 1;
    }

    #[inline]
    pub fn set(&mut self, index: usize, value: T) {
        assert!(index < self.len, "spool index out of bounds");
        self.write_at(index, value);
    }

    #[inline]
    fn write_at(&mut self, index: usize, value: T) {
        let elem_len = std::mem::size_of::<T>();
        let start = index * elem_len;
        let bytes =
            unsafe { std::slice::from_raw_parts((&value as *const T).cast::<u8>(), elem_len) };
        self.mmap[start..start + elem_len].copy_from_slice(bytes);
    }

    #[inline]
    pub fn get(&self, index: usize) -> T {
        assert!(index < self.len, "spool index out of bounds");
        let elem_len = std::mem::size_of::<T>();
        let start = index * elem_len;
        unsafe { std::ptr::read_unaligned(self.mmap[start..].as_ptr().cast::<T>()) }
    }

    /// Pops and returns the last element, or `None` if empty. Together with
    /// [`Spool::last`] and [`Spool::set_last`], lets a `Spool` stand in for a
    /// `Vec`-backed explicit traversal stack.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let value = self.get(self.len - 1);
        self.len -= 1;
        Some(value)
    }

    #[inline]
    pub fn last(&self) -> Option<T> {
        if self.len == 0 {
            None
        } else {
            Some(self.get(self.len - 1))
        }
    }

    #[inline]
    pub fn set_last(&mut self, value: T) {
        let idx = self.len - 1;
        self.set(idx, value);
    }

    /// Logically empties the spool without releasing its reserved capacity —
    /// mirrors `Vec::clear`.
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The written prefix, as a slice. Valid because `Spool<T>` is mapped
    /// page-aligned (far stricter than any `T` used here needs) and every
    /// byte in `0..len * size_of::<T>()` was written by `push`/`set`.
    pub fn as_slice(&self) -> &[T] {
        unsafe { std::slice::from_raw_parts(self.mmap.as_ptr().cast::<T>(), self.len) }
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl<T: Pod> Drop for Spool<T> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A growable, file-backed byte buffer for data whose final length isn't
/// known up front — compaction has exactly one such column, the front-coded
/// name blob. Doubles capacity on overflow, like `Vec`, but by remapping a
/// resized file instead of reallocating heap.
pub struct ByteSpool {
    file: File,
    mmap: MmapMut,
    len: usize,
    capacity: usize,
    path: PathBuf,
}

impl ByteSpool {
    pub fn create(
        path: &Path,
        initial_capacity: usize,
        on_create: impl FnOnce(&File),
    ) -> io::Result<Self> {
        let capacity = initial_capacity.max(4096);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        on_create(&file);
        file.set_len(capacity as u64)?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        Ok(ByteSpool {
            file,
            mmap,
            len: 0,
            capacity,
            path: path.to_path_buf(),
        })
    }

    pub fn push(&mut self, byte: u8) {
        self.reserve(1);
        self.mmap[self.len] = byte;
        self.len += 1;
    }

    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.reserve(bytes.len());
        self.mmap[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    fn reserve(&mut self, additional: usize) {
        if self.len + additional <= self.capacity {
            return;
        }
        let mut new_capacity = self.capacity.max(4096);
        while new_capacity < self.len + additional {
            new_capacity *= 2;
        }
        // Windows refuses to extend a file that has an active mapped view
        // over it, so the old mapping must be dropped (replaced, here, by a
        // throwaway one-page anonymous mapping) before `set_len` runs.
        self.mmap = MmapMut::map_anon(1).expect("temporary anonymous mapping");
        self.file
            .set_len(new_capacity as u64)
            .expect("grow spool file");
        self.mmap = unsafe { MmapMut::map_mut(&self.file).expect("remap spool file") };
        self.capacity = new_capacity;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.mmap[..self.len]
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ByteSpool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spool_round_trips_pushed_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.spool");
        let mut spool: Spool<u32> = Spool::create(&path, 10, |_| {}).unwrap();
        for i in 0..10u32 {
            spool.push(i * 3);
        }
        assert_eq!(spool.as_slice(), &[0, 3, 6, 9, 12, 15, 18, 21, 24, 27]);
        spool.set(5, 999);
        assert_eq!(spool.get(5), 999);
        assert_eq!(spool.len(), 10);
    }

    #[test]
    fn spool_file_is_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dropped.spool");
        {
            let mut spool: Spool<u8> = Spool::create(&path, 4, |_| {}).unwrap();
            spool.push(1);
        }
        assert!(!path.exists());
    }

    #[test]
    fn byte_spool_grows_past_initial_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bytespool");
        let mut spool = ByteSpool::create(&path, 4, |_| {}).unwrap();
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 256) as u8).collect();
        spool.extend_from_slice(&payload);
        assert_eq!(spool.as_slice(), payload.as_slice());
        assert_eq!(spool.len(), payload.len());
    }

    #[test]
    fn byte_spool_push_and_extend_agree_with_a_vec_reference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.bytespool");
        let mut spool = ByteSpool::create(&path, 4, |_| {}).unwrap();
        let mut reference = Vec::new();
        for i in 0..2000u32 {
            if i % 7 == 0 {
                spool.push(i as u8);
                reference.push(i as u8);
            } else {
                let chunk = [i as u8, (i >> 8) as u8];
                spool.extend_from_slice(&chunk);
                reference.extend_from_slice(&chunk);
            }
        }
        assert_eq!(spool.as_slice(), reference.as_slice());
    }
}
