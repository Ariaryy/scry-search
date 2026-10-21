//! Raw-MFT and USN enumeration assembled into an initial index.

use super::records::{decode_name_into, for_each_usn_record};
use super::{ffi, open_volume, WindowsBackendError};
use scry_core::record::filetime_to_secs;
use scry_core::ArenaBuilder;
use std::ffi::c_void;

struct IndexAssembly<'a> {
    volume: &'a str,
    builder: ArenaBuilder,
    frn_table: Vec<(u64, u32)>,
    parent_frns: Vec<u64>,
}

struct IndexMetadata {
    is_dir: bool,
    mtime_secs: u32,
    size_bytes: u64,
    size_exact: bool,
}

impl<'a> IndexAssembly<'a> {
    fn new(volume: &'a str) -> Self {
        Self {
            volume,
            builder: ArenaBuilder::with_capacity(200_000, 4 * 1024 * 1024),
            frn_table: Vec::new(),
            parent_frns: Vec::new(),
        }
    }

    fn push(&mut self, frn: u64, parent_frn: u64, name: &[u8], metadata: IndexMetadata) {
        let index = self.builder.push_bytes_with_metadata_exact(
            name,
            metadata.mtime_secs,
            metadata.is_dir,
            Some(frn),
            metadata.size_bytes,
            metadata.size_exact,
        );
        self.frn_table.push((frn, index));
        self.parent_frns.push(parent_frn);
    }

    fn finish(mut self) -> (scry_core::Arena, Vec<scry_core::frnmap::FrnEntry>) {
        let root = self.builder.push(self.volume, 0, true);
        self.frn_table.sort_unstable_by_key(|&(frn, _)| frn);
        for (index, &parent_frn) in self.parent_frns.iter().enumerate() {
            match self
                .frn_table
                .binary_search_by_key(&parent_frn, |&(frn, _)| frn)
            {
                Ok(position) if self.frn_table[position].1 != index as u32 => {
                    self.builder
                        .set_parent(index as u32, self.frn_table[position].1);
                }
                _ => self.builder.set_parent(index as u32, root),
            }
        }
        self.builder.build()
    }
}

pub(super) fn build_raw_index(
    volume: &str,
) -> Result<
    (
        scry_core::Arena,
        Vec<scry_core::frnmap::FrnEntry>,
        crate::mft::MftEnumReport,
    ),
    crate::mft::MftError,
> {
    let mut assembly = IndexAssembly::new(volume);
    let report = crate::mft::enumerate_mft_raw(
        volume,
        |frn, parent, name, is_dir, mtime, size, size_exact| {
            assembly.push(
                frn,
                parent,
                name,
                IndexMetadata {
                    is_dir,
                    mtime_secs: mtime,
                    size_bytes: size,
                    size_exact,
                },
            );
        },
    )?;
    if report.torn_records_skipped > report.emitted / 100 {
        return Err(crate::mft::MftError::Invalid("excessive torn-record rate"));
    }
    let (arena, frns) = assembly.finish();
    Ok((arena, frns, report))
}

pub(super) fn build_usn_index(
    volume: &str,
) -> Result<(scry_core::Arena, Vec<scry_core::frnmap::FrnEntry>), WindowsBackendError> {
    let mut assembly = IndexAssembly::new(volume);
    enumerate_mft_usn(volume, |frn, parent, name, is_dir, mtime, size| {
        assembly.push(
            frn,
            parent,
            name,
            IndexMetadata {
                is_dir,
                mtime_secs: mtime,
                size_bytes: size,
                size_exact: false,
            },
        );
    })?;
    Ok(assembly.finish())
}

/// Bulk-enumerates the volume's MFT, handing each entry to `sink` as it is
/// parsed. Streaming rather than returning a `Vec<RawEntry>` is what keeps a
/// rebuild from holding a `String` per file: the caller copies the name into
/// its own staging blob and the ioctl buffer is immediately reused.
///
/// `sink` receives: FRN, parent FRN, UTF-8 name bytes, is-directory, and the
/// raw FILETIME mtime.
pub fn enumerate_mft_usn(
    volume: &str,
    mut sink: impl FnMut(u64, u64, &[u8], bool, u32, u64),
) -> Result<(), WindowsBackendError> {
    let handle = open_volume(volume, 0)?;

    let result = (|| {
        let mut start_frn: u64 = 0;
        // 1 MiB output buffer. The ioctl's per-call cost is a kernel
        // transition plus MFT seek setup, so a larger buffer directly
        // reduces syscall count and wall time on a full enumeration. One
        // megabyte is immaterial against the ~14 MB steady-state index; the
        // earlier 64 KiB choice was optimising the wrong side of that
        // tradeoff.
        let mut out_buf = vec![0u8; 1024 * 1024];
        let mut name = String::new();

        loop {
            let input = ffi::MftEnumDataV0 {
                start_file_reference_number: start_frn,
                low_usn: 0,
                high_usn: i64::MAX,
            };
            let mut bytes_returned: u32 = 0;

            crate::throttle::acquire(out_buf.len());
            let ok = unsafe {
                ffi::DeviceIoControl(
                    handle,
                    ffi::FSCTL_ENUM_USN_DATA,
                    &input as *const _ as *const c_void,
                    std::mem::size_of::<ffi::MftEnumDataV0>() as u32,
                    out_buf.as_mut_ptr() as *mut c_void,
                    out_buf.len() as u32,
                    &mut bytes_returned,
                    std::ptr::null_mut(),
                )
            };

            if ok == 0 {
                let code = unsafe { ffi::GetLastError() };
                if code == ffi::ERROR_HANDLE_EOF {
                    break;
                }
                return Err(WindowsBackendError::Enumerate { code });
            }
            if bytes_returned < 8 {
                break;
            }

            // First 8 bytes: FRN to resume from on the next call.
            let next_start = u64::from_ne_bytes(out_buf[0..8].try_into().unwrap());

            for_each_usn_record(&out_buf, bytes_returned as usize, |header, name_bytes| {
                decode_name_into(name_bytes, &mut name);
                sink(
                    header.file_reference_number,
                    header.parent_file_reference_number,
                    name.as_bytes(),
                    header.file_attributes & ffi::FILE_ATTRIBUTE_DIRECTORY != 0,
                    filetime_to_secs(header.time_stamp),
                    0,
                );
            });

            if next_start == start_frn {
                break;
            }
            start_frn = next_start;
        }

        Ok(())
    })();

    unsafe { ffi::CloseHandle(handle) };
    result
}
