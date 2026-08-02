use crate::ffi;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;

/// Read-only-shareable section backed by either the pagefile or an immutable file.
pub struct Section {
    handle: ffi::Handle,
    len: usize,
}

/// Read-only mapping of a section in the current process.
pub struct SectionView {
    handle: ffi::Handle,
    base: *const u8,
    len: usize,
}

// SAFETY: kernel section handles have no thread affinity and `Section` exposes
// no writable mapping after construction.
unsafe impl Send for Section {}
unsafe impl Sync for Section {}
// SAFETY: the view is immutable for its lifetime; the owned handle and mapping
// are released exactly once by Drop.
unsafe impl Send for SectionView {}
unsafe impl Sync for SectionView {}

impl Section {
    pub fn create(bytes: &[u8]) -> io::Result<Self> {
        if bytes.is_empty() || bytes.len() > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid section length",
            ));
        }
        let handle = unsafe {
            ffi::CreateFileMappingW(
                ffi::INVALID_HANDLE_VALUE,
                std::ptr::null_mut(),
                ffi::PAGE_READWRITE,
                0,
                bytes.len() as u32,
                std::ptr::null(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let base = unsafe { ffi::MapViewOfFile(handle, ffi::FILE_MAP_WRITE, 0, 0, bytes.len()) };
        if base.is_null() {
            unsafe { ffi::CloseHandle(handle) };
            return Err(io::Error::last_os_error());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.cast::<u8>(), bytes.len());
            ffi::UnmapViewOfFile(base);
        }
        Ok(Self {
            handle,
            len: bytes.len(),
        })
    }

    pub fn from_file(file: &File) -> io::Result<Self> {
        let len = usize::try_from(file.metadata()?.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "section file is too large")
        })?;
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty section file",
            ));
        }
        let handle = unsafe {
            ffi::CreateFileMappingW(
                file.as_raw_handle().cast(),
                std::ptr::null_mut(),
                ffi::PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle, len })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn duplicate_for(&self, process_id: u32) -> io::Result<u64> {
        let process = unsafe { ffi::OpenProcess(ffi::PROCESS_DUP_HANDLE, 0, process_id) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut duplicated = std::ptr::null_mut();
        let ok = unsafe {
            ffi::DuplicateHandle(
                ffi::GetCurrentProcess(),
                self.handle,
                process,
                &mut duplicated,
                ffi::FILE_MAP_READ | ffi::SECTION_QUERY,
                0,
                0,
            )
        };
        unsafe { ffi::CloseHandle(process) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(duplicated as usize as u64)
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        unsafe { ffi::CloseHandle(self.handle) };
    }
}

impl SectionView {
    /// Takes ownership of `handle` even when mapping fails.
    pub fn map(handle: u64, len: usize) -> io::Result<Self> {
        let handle = handle as usize as ffi::Handle;
        let base = unsafe { ffi::MapViewOfFile(handle, ffi::FILE_MAP_READ, 0, 0, len) };
        if base.is_null() {
            unsafe { ffi::CloseHandle(handle) };
            return Err(io::Error::last_os_error());
        }
        if !(base as usize).is_multiple_of(std::mem::align_of::<u64>()) {
            unsafe {
                ffi::UnmapViewOfFile(base);
                ffi::CloseHandle(handle);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unaligned section view",
            ));
        }
        Ok(Self {
            handle,
            base: base.cast(),
            len,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base, self.len) }
    }
}

impl Drop for SectionView {
    fn drop(&mut self) {
        unsafe {
            ffi::UnmapViewOfFile(self.base.cast::<c_void>());
            ffi::CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_roundtrip_in_process() {
        let bytes: Vec<u8> = (0..1_048_576).map(|i| (i % 251) as u8).collect();
        let section = Section::create(&bytes).unwrap();
        let handle = section.duplicate_for(std::process::id()).unwrap();
        let view = SectionView::map(handle, section.len()).unwrap();
        assert_eq!(view.as_bytes(), bytes);
        assert_eq!(
            view.as_bytes().as_ptr() as usize % std::mem::align_of::<u64>(),
            0
        );
    }

    #[test]
    fn file_backed_section_roundtrip_in_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("section.bin");
        let bytes: Vec<u8> = (0..1_048_576).map(|i| (i % 239) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let file = File::open(path).unwrap();
        let section = Section::from_file(&file).unwrap();
        let handle = section.duplicate_for(std::process::id()).unwrap();
        let view = SectionView::map(handle, section.len()).unwrap();
        assert_eq!(view.as_bytes(), bytes);
    }

    fn handle_count() -> u32 {
        let mut count = 0;
        let ok = unsafe { ffi::GetProcessHandleCount(ffi::GetCurrentProcess(), &mut count) };
        assert_ne!(ok, 0);
        count
    }

    #[test]
    fn section_drop_closes_handle() {
        let before = handle_count();
        for _ in 0..10_000 {
            let section = Section::create(&[1]).unwrap();
            let handle = section.duplicate_for(std::process::id()).unwrap();
            drop(SectionView::map(handle, 1).unwrap());
        }
        let after = handle_count();
        assert!(
            after <= before + 2,
            "handle count grew from {before} to {after}"
        );
    }
}
