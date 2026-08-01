//! Bounds-checked raw NTFS metadata reader.

pub mod attr;
pub mod boot;
pub mod record;
pub mod runlist;

use thiserror::Error;

use std::io::Seek;
use std::os::windows::fs::OpenOptionsExt;

use attr::{
    data_size, has_attribute_list, parse_file_name, parse_standard_information, DATA, FILE_NAME,
    STANDARD_INFORMATION,
};
use boot::{read_volume_boot, VolumeGeometry};
use record::ParsedRecord;
use runlist::{decode_runs, Run};

#[derive(Debug, Error)]
pub enum MftError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid NTFS structure: {0}")]
    Invalid(&'static str),
    #[error("torn NTFS FILE record")]
    TornRecord,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MftEnumReport {
    pub emitted: usize,
    pub torn_records_skipped: usize,
    pub bad_records_skipped: usize,
    pub attribute_list_fallbacks: usize,
    pub extra_hard_links_ignored: usize,
}

pub fn enumerate_mft_raw(
    volume: &str,
    mut sink: impl FnMut(u64, u64, &[u8], bool, u32, u64),
) -> Result<MftEnumReport, MftError> {
    let geometry = read_volume_boot(volume)?;
    let path = format!(r"\\.\{volume}");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1 | 2)
        .open(path)?;
    let (runs, stream_size) = mft_runs(&mut file, geometry)?;
    drop(file);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1 | 2)
        .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(format!(r"\\.\{volume}"))?;
    let mut report = MftEnumReport::default();
    let mut buffer = AlignedBuffer::new(4 * 1024 * 1024, geometry.physical_sector_size)?;
    let mut stream_position = 0u64;

    for run in runs {
        let Some(lcn) = run.lcn else {
            return Err(MftError::Invalid("sparse run in MFT data"));
        };
        let disk_offset = (lcn as u64)
            .checked_mul(geometry.cluster_size as u64)
            .ok_or(MftError::Invalid("MFT run offset overflow"))?;
        let run_bytes = run
            .cluster_count
            .checked_mul(geometry.cluster_size as u64)
            .ok_or(MftError::Invalid("MFT run length overflow"))?;
        let mut run_position = 0u64;
        while run_position < run_bytes && stream_position < stream_size {
            let remaining_run = run_bytes - run_position;
            let remaining_stream = stream_size - stream_position;
            let wanted = remaining_run.min(buffer.len() as u64) as usize;
            let aligned = wanted - wanted % geometry.physical_sector_size as usize;
            if aligned == 0 {
                break;
            }
            file.seek(std::io::SeekFrom::Start(disk_offset + run_position))?;
            std::io::Read::read_exact(&mut file, &mut buffer.as_mut_slice()[..aligned])?;
            let logical = (remaining_stream.min(aligned as u64) as usize)
                / geometry.file_record_size as usize
                * geometry.file_record_size as usize;
            for (within_chunk, record_bytes) in buffer.as_mut_slice()[..logical]
                .chunks_exact_mut(geometry.file_record_size as usize)
                .enumerate()
            {
                let stream_record =
                    stream_position / geometry.file_record_size as u64 + within_chunk as u64;
                match parse_and_emit(
                    record_bytes,
                    geometry,
                    stream_record,
                    &mut sink,
                    &mut report,
                ) {
                    Ok(()) => {}
                    Err(MftError::TornRecord) => report.torn_records_skipped += 1,
                    Err(error) => return Err(error),
                }
            }
            run_position += aligned as u64;
            stream_position += aligned as u64;
        }
    }
    Ok(report)
}

const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_RELEASE: u32 = 0x8000;
const PAGE_READWRITE: u32 = 0x04;

struct AlignedBuffer {
    pointer: *mut u8,
    length: usize,
}

impl AlignedBuffer {
    fn new(length: usize, alignment: u32) -> Result<Self, MftError> {
        let pointer = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                length,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        }
        .cast::<u8>();
        if pointer.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        if !(pointer as usize).is_multiple_of(alignment as usize) {
            unsafe {
                VirtualFree(pointer.cast(), 0, MEM_RELEASE);
            }
            return Err(MftError::Invalid(
                "VirtualAlloc buffer is not sector-aligned",
            ));
        }
        Ok(Self { pointer, length })
    }

    fn len(&self) -> usize {
        self.length
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the allocation owns `length` writable bytes and this method
        // requires the unique mutable borrow of the owner.
        unsafe { std::slice::from_raw_parts_mut(self.pointer, self.length) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe {
            VirtualFree(self.pointer.cast(), 0, MEM_RELEASE);
        }
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn VirtualAlloc(
        address: *mut std::ffi::c_void,
        size: usize,
        allocation_type: u32,
        protect: u32,
    ) -> *mut std::ffi::c_void;
    fn VirtualFree(address: *mut std::ffi::c_void, size: usize, free_type: u32) -> i32;
}

fn mft_runs(
    file: &mut std::fs::File,
    geometry: VolumeGeometry,
) -> Result<(Vec<Run>, u64), MftError> {
    let offset = geometry
        .mft_offset()
        .ok_or(MftError::Invalid("MFT offset overflow"))?;
    file.seek(std::io::SeekFrom::Start(offset))?;
    let mut bytes = vec![0u8; geometry.file_record_size as usize];
    std::io::Read::read_exact(file, &mut bytes)?;
    let record = ParsedRecord::parse(&mut bytes, geometry.bytes_per_sector as usize)?
        .ok_or(MftError::Invalid("MFT record zero is not a FILE record"))?;
    for attribute in record.attributes() {
        let attribute = attribute?;
        if attribute.type_code == DATA && attribute.non_resident && unnamed(&attribute)? {
            let run_offset = record::read_u16(attribute.bytes, 0x20)? as usize;
            let run_bytes = attribute
                .bytes
                .get(run_offset..)
                .ok_or(MftError::Invalid("MFT run list offset out of bounds"))?;
            let stream_size = record::read_u64(attribute.bytes, 0x30)?;
            return Ok((decode_runs(run_bytes)?, stream_size));
        }
    }
    Err(MftError::Invalid(
        "MFT record has no unnamed non-resident DATA",
    ))
}

fn unnamed(attribute: &record::AttributeRef<'_>) -> Result<bool, MftError> {
    Ok(*attribute
        .bytes
        .get(9)
        .ok_or(MftError::Invalid("truncated attribute name length"))?
        == 0)
}

fn parse_and_emit(
    bytes: &mut [u8],
    geometry: VolumeGeometry,
    stream_record: u64,
    sink: &mut impl FnMut(u64, u64, &[u8], bool, u32, u64),
    report: &mut MftEnumReport,
) -> Result<(), MftError> {
    let Some(record) = ParsedRecord::parse(bytes, geometry.bytes_per_sector as usize)? else {
        report.bad_records_skipped += 1;
        return Ok(());
    };
    if !record.in_use() || record.is_extension() {
        return Ok(());
    }
    if record.record_number() as u64 != stream_record {
        return Err(MftError::Invalid(
            "FILE record number does not match stream position",
        ));
    }
    let mut selected_name = None;
    let mut fallback_name = None;
    let mut mtime = 0u32;
    let mut size = 0u64;
    for attribute in record.attributes() {
        let attribute = attribute?;
        if has_attribute_list(&attribute) {
            report.attribute_list_fallbacks += 1;
        } else if attribute.type_code == STANDARD_INFORMATION {
            mtime = parse_standard_information(&attribute)?;
        } else if attribute.type_code == FILE_NAME {
            let name = parse_file_name(&attribute)?;
            if name.namespace == 2 {
                continue;
            }
            if name.namespace == 1 || name.namespace == 3 {
                if selected_name.is_some() {
                    report.extra_hard_links_ignored += 1;
                } else {
                    selected_name = Some(name);
                }
            } else if fallback_name.is_none() {
                fallback_name = Some(name);
            }
        } else if attribute.type_code == DATA && unnamed(&attribute)? {
            size = data_size(&attribute)?;
        }
    }
    let Some(name) = selected_name.or(fallback_name) else {
        return Ok(());
    };
    sink(
        record.frn(),
        name.parent_frn,
        name.name.as_bytes(),
        record.is_dir(),
        mtime,
        size,
    );
    report.emitted += 1;
    Ok(())
}
