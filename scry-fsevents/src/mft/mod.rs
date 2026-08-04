//! Bounds-checked raw NTFS metadata reader.

pub mod attr;
pub mod boot;
pub mod record;
pub mod runlist;

use thiserror::Error;

use std::io::Seek;
use std::os::windows::fs::OpenOptionsExt;

use attr::{
    attribute_list_entries, data_size, has_attribute_list, parse_file_name,
    parse_standard_information, DATA, FILE_NAME, STANDARD_INFORMATION,
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
    pub attribute_list_resolved: usize,
    pub attribute_list_unresolved: usize,
    pub nonresident_attribute_lists: usize,
    pub targeted_extension_reads: usize,
    pub extra_hard_links_ignored: usize,
    /// Non-directory records emitted with a size of zero.
    ///
    /// A genuinely empty file is rare, so this should stay near zero on a real
    /// volume. A large count means `$DATA` is not being found — the usual
    /// causes being a base record whose unnamed `$DATA` lives in an extension
    /// reached through an `$ATTRIBUTE_LIST` we could not resolve, or a stream
    /// that is entirely named. `size` reaching the index as 0 is documented to
    /// mean *unknown*, so without this counter the gap is indistinguishable
    /// from a volume full of empty files.
    pub files_with_zero_size: usize,
}

/// Records one emitted entry. Both emit sites go through here so the size
/// coverage counter can't drift away from `emitted`.
fn note_emitted(report: &mut MftEnumReport, is_dir: bool, size: u64) {
    report.emitted += 1;
    if !is_dir && size == 0 {
        report.files_with_zero_size += 1;
    }
}

struct DeferredRecord {
    frn: u64,
    child_references: Vec<u64>,
    names: Vec<attr::FileNameInfo>,
    is_dir: bool,
    mtime: u32,
    size: u64,
}

struct ExtensionRecord {
    base_reference: u64,
    names: Vec<attr::FileNameInfo>,
    size: Option<u64>,
    mtime: Option<u32>,
}

#[derive(Default)]
struct PassState {
    report: MftEnumReport,
    deferred: Vec<DeferredRecord>,
    extensions: std::collections::HashMap<u64, ExtensionRecord>,
}

pub fn enumerate_mft_raw(
    volume: &str,
    mut sink: impl FnMut(u64, u64, &[u8], bool, u32, u64),
) -> Result<MftEnumReport, MftError> {
    enumerate_mft_raw_with_names(volume, &mut sink, |_, _, _| {})
}

#[doc(hidden)]
pub fn enumerate_mft_raw_with_names(
    volume: &str,
    mut sink: impl FnMut(u64, u64, &[u8], bool, u32, u64),
    mut name_sink: impl FnMut(u64, &[attr::FileNameInfo], bool),
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
    let mut state = PassState::default();
    let mut buffer = AlignedBuffer::new(4 * 1024 * 1024, geometry.physical_sector_size)?;
    let mut stream_position = 0u64;
    let run_count = runs.len();
    let mut read_elapsed = std::time::Duration::ZERO;
    let mut parse_elapsed = std::time::Duration::ZERO;

    for run in &runs {
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
            crate::throttle::acquire(aligned);
            let read_started = std::time::Instant::now();
            std::io::Read::read_exact(&mut file, &mut buffer.as_mut_slice()[..aligned])?;
            read_elapsed += read_started.elapsed();
            let logical = (remaining_stream.min(aligned as u64) as usize)
                / geometry.file_record_size as usize
                * geometry.file_record_size as usize;
            let parse_started = std::time::Instant::now();
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
                    &mut name_sink,
                    &mut state,
                ) {
                    Ok(()) => {}
                    Err(MftError::TornRecord) => state.report.torn_records_skipped += 1,
                    Err(error) => {
                        eprintln!(
                            "scry: raw MFT parse failed at record {stream_record}, \
                             stream byte {stream_position}, run LCN {lcn}, run byte {run_position}: {error}"
                        );
                        return Err(error);
                    }
                }
            }
            parse_elapsed += parse_started.elapsed();
            run_position += aligned as u64;
            stream_position += aligned as u64;
        }
    }
    resolve_deferred(
        volume,
        geometry,
        &runs,
        &mut sink,
        &mut name_sink,
        &mut state,
    )?;
    eprintln!("scry: raw MFT runs={run_count}, read={read_elapsed:?}, parse={parse_elapsed:?}");
    Ok(state.report)
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
    name_sink: &mut impl FnMut(u64, &[attr::FileNameInfo], bool),
    state: &mut PassState,
) -> Result<(), MftError> {
    let Some(record) = ParsedRecord::parse(bytes, geometry.bytes_per_sector as usize)? else {
        state.report.bad_records_skipped += 1;
        return Ok(());
    };
    if !record.in_use() {
        return Ok(());
    }
    if record.record_number() as u64 != stream_record {
        state.report.bad_records_skipped += 1;
        return Ok(());
    }
    if record.is_extension() {
        state
            .extensions
            .insert(record.frn(), parse_extension(&record)?);
        return Ok(());
    }
    let mut win32_names = Vec::new();
    let mut fallback_names = Vec::new();
    let mut mtime = 0u32;
    let mut size = 0u64;
    let mut list_entries = Vec::new();
    let mut saw_attribute_list = false;
    let mut nonresident_attribute_list = false;
    let mut has_standard_information = false;
    let mut has_data = false;
    for attribute in record.attributes() {
        let attribute = attribute?;
        if has_attribute_list(&attribute) {
            state.report.attribute_list_fallbacks += 1;
            saw_attribute_list = true;
            match attribute_list_entries(&attribute)? {
                Some(entries) => list_entries.extend(entries),
                None => {
                    state.report.nonresident_attribute_lists += 1;
                    nonresident_attribute_list = true;
                }
            }
        } else if attribute.type_code == STANDARD_INFORMATION {
            mtime = parse_standard_information(&attribute)?;
            has_standard_information = true;
        } else if attribute.type_code == FILE_NAME {
            let name = parse_file_name(&attribute)?;
            if name.namespace == 2 {
                continue;
            }
            if name.namespace == 1 || name.namespace == 3 {
                win32_names.push(name);
            } else {
                fallback_names.push(name);
            }
        } else if attribute.type_code == DATA && unnamed(&attribute)? {
            size = data_size(&attribute)?;
            has_data = true;
        }
    }
    let mut names = win32_names;
    names.extend(fallback_names);
    let mut child_references = list_entries
        .into_iter()
        .filter(|entry| entry.file_reference & 0x0000_ffff_ffff_ffff != stream_record)
        .filter(|entry| {
            (entry.type_code == FILE_NAME && names.is_empty())
                || (entry.type_code == DATA && !has_data)
                || (entry.type_code == STANDARD_INFORMATION && !has_standard_information)
        })
        .map(|entry| entry.file_reference)
        .collect::<Vec<_>>();
    child_references.sort_unstable();
    child_references.dedup();
    if saw_attribute_list && (!child_references.is_empty() || names.is_empty()) {
        state.deferred.push(DeferredRecord {
            frn: record.frn(),
            child_references,
            names,
            is_dir: record.is_dir(),
            mtime,
            size,
        });
        return Ok(());
    }
    let oracle_count = names
        .iter()
        .take_while(|name| name.namespace == 1 || name.namespace == 3)
        .count();
    name_sink(
        record.frn(),
        &names[..oracle_count],
        !nonresident_attribute_list,
    );
    state.report.extra_hard_links_ignored += names.len().saturating_sub(1);
    let Some(name) = names.first() else {
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
    note_emitted(&mut state.report, record.is_dir(), size);
    Ok(())
}

fn resolve_deferred(
    volume: &str,
    geometry: VolumeGeometry,
    runs: &[Run],
    sink: &mut impl FnMut(u64, u64, &[u8], bool, u32, u64),
    name_sink: &mut impl FnMut(u64, &[attr::FileNameInfo], bool),
    state: &mut PassState,
) -> Result<(), MftError> {
    use std::collections::BTreeMap;

    let mut children = BTreeMap::<(u64, u64), Vec<usize>>::new();
    for (base_index, base) in state.deferred.iter().enumerate() {
        for reference in &base.child_references {
            children
                .entry((reference & 0x0000_ffff_ffff_ffff, *reference))
                .or_default()
                .push(base_index);
        }
    }
    let mut resolved_names = (0..state.deferred.len())
        .map(|_| Vec::new())
        .collect::<Vec<_>>();
    let mut resolved_sizes = vec![None; state.deferred.len()];
    let mut resolved_mtimes = vec![None; state.deferred.len()];
    let mut resolved_children = vec![false; state.deferred.len()];
    let mut file = None;
    let mut bytes = vec![0u8; geometry.file_record_size as usize];
    for ((record_number, reference), base_indexes) in children {
        let fallback;
        let extension = if let Some(extension) = state.extensions.get(&reference) {
            extension
        } else {
            state.report.targeted_extension_reads += 1;
            let offset = record_disk_offset(record_number, geometry, runs)?;
            let handle = match &mut file {
                Some(handle) => handle,
                None => file.insert(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .share_mode(1 | 2)
                        .open(format!(r"\\.\{volume}"))?,
                ),
            };
            handle.seek(std::io::SeekFrom::Start(offset))?;
            std::io::Read::read_exact(handle, &mut bytes)?;
            let Some(record) = ParsedRecord::parse(&mut bytes, geometry.bytes_per_sector as usize)?
            else {
                continue;
            };
            if !record.in_use() || !record.is_extension() || record.frn() != reference {
                continue;
            }
            fallback = parse_extension(&record)?;
            &fallback
        };
        for base_index in base_indexes {
            let base = &state.deferred[base_index];
            if extension.base_reference == base.frn {
                resolved_children[base_index] = true;
                if extension.size.is_some() {
                    resolved_sizes[base_index] = extension.size;
                }
                if extension.mtime.is_some() {
                    resolved_mtimes[base_index] = extension.mtime;
                }
                resolved_names[base_index].extend(extension.names.iter().map(|name| {
                    attr::FileNameInfo {
                        parent_frn: name.parent_frn,
                        namespace: name.namespace,
                        name: name.name.clone(),
                    }
                }));
            }
        }
    }
    for (base_index, (base, extension_names)) in
        state.deferred.iter().zip(resolved_names).enumerate()
    {
        let mut names = base
            .names
            .iter()
            .map(|name| attr::FileNameInfo {
                parent_frn: name.parent_frn,
                namespace: name.namespace,
                name: name.name.clone(),
            })
            .collect::<Vec<_>>();
        names.extend(extension_names);
        let win32_names = names
            .iter()
            .filter(|name| name.namespace == 1 || name.namespace == 3)
            .collect::<Vec<_>>();
        let oracle_names = win32_names
            .iter()
            .map(|name| attr::FileNameInfo {
                parent_frn: name.parent_frn,
                namespace: name.namespace,
                name: name.name.clone(),
            })
            .collect::<Vec<_>>();
        name_sink(base.frn, &oracle_names, resolved_children[base_index]);
        let selected = win32_names
            .first()
            .copied()
            .or_else(|| names.iter().find(|name| name.namespace == 0));
        if let Some(name) = selected {
            state.report.extra_hard_links_ignored += names.len().saturating_sub(1);
            let size = resolved_sizes[base_index].unwrap_or(base.size);
            sink(
                base.frn,
                name.parent_frn,
                name.name.as_bytes(),
                base.is_dir,
                resolved_mtimes[base_index].unwrap_or(base.mtime),
                size,
            );
            note_emitted(&mut state.report, base.is_dir, size);
        }
        if resolved_children[base_index] {
            state.report.attribute_list_resolved += 1;
        } else {
            state.report.attribute_list_unresolved += 1;
        }
    }
    Ok(())
}

fn parse_extension(record: &ParsedRecord<'_>) -> Result<ExtensionRecord, MftError> {
    let mut names = Vec::new();
    let mut size = None;
    let mut mtime = None;
    for attribute in record.attributes() {
        let attribute = attribute?;
        if attribute.type_code == FILE_NAME {
            let name = parse_file_name(&attribute)?;
            if name.namespace != 2 {
                names.push(name);
            }
        } else if attribute.type_code == DATA && unnamed(&attribute)? {
            size = Some(data_size(&attribute)?);
        } else if attribute.type_code == STANDARD_INFORMATION {
            mtime = Some(parse_standard_information(&attribute)?);
        }
    }
    Ok(ExtensionRecord {
        base_reference: record.base_reference(),
        names,
        size,
        mtime,
    })
}

fn record_disk_offset(
    record_number: u64,
    geometry: VolumeGeometry,
    runs: &[Run],
) -> Result<u64, MftError> {
    let logical = record_number
        .checked_mul(geometry.file_record_size as u64)
        .ok_or(MftError::Invalid("MFT record position overflow"))?;
    let mut run_start = 0u64;
    for run in runs {
        let run_bytes = run
            .cluster_count
            .checked_mul(geometry.cluster_size as u64)
            .ok_or(MftError::Invalid("MFT run length overflow"))?;
        if logical >= run_start && logical - run_start < run_bytes {
            let within = logical - run_start;
            if within + geometry.file_record_size as u64 > run_bytes {
                return Err(MftError::Invalid("MFT record crosses a data run"));
            }
            let lcn = run.lcn.ok_or(MftError::Invalid("sparse run in MFT data"))?;
            return (lcn as u64)
                .checked_mul(geometry.cluster_size as u64)
                .and_then(|offset| offset.checked_add(within))
                .ok_or(MftError::Invalid("MFT record disk offset overflow"));
        }
        run_start = run_start
            .checked_add(run_bytes)
            .ok_or(MftError::Invalid("MFT logical run offset overflow"))?;
    }
    Err(MftError::Invalid("MFT record lies outside data runs"))
}
