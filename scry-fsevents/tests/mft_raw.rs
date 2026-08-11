#![cfg(windows)]

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;

#[test]
#[ignore = "elevated aggregate diagnostic over a live NTFS volume"]
fn classify_unknown_size_records() {
    const SAMPLE_LIMIT: usize = 2_000;
    let mut sample = Vec::with_capacity(SAMPLE_LIMIT);
    let mut seen = 0u64;
    let mut random = 0x6a09_e667_f3bc_c909u64;
    let report =
        scry_fsevents::mft::enumerate_mft_raw("C:", |frn, _, _, is_dir, _, _, size_exact| {
            if size_exact {
                return;
            }
            seen += 1;
            let slot = if sample.len() < SAMPLE_LIMIT {
                sample.push((frn, is_dir));
                None
            } else {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let candidate = random % seen;
                (candidate < SAMPLE_LIMIT as u64).then_some(candidate as usize)
            };
            if let Some(slot) = slot {
                sample[slot] = (frn, is_dir);
            }
        })
        .expect("raw MFT enumeration requires elevation and supported NTFS");
    let buckets = classify_unknown_sample(&sample);
    println!(
        "sampled={} deleted_during_probe={} reparse={} sparse={} encrypted={} directory={} no_unnamed_data={} other_readable={} open_failure={} raw_emitted={}",
        sample.len(),
        buckets.deleted,
        buckets.reparse,
        buckets.sparse,
        buckets.encrypted,
        buckets.directory,
        buckets.no_unnamed_data,
        buckets.other_readable,
        buckets.open_failure,
        report.emitted,
    );
}

#[derive(Default)]
struct UnknownBuckets {
    deleted: usize,
    reparse: usize,
    sparse: usize,
    encrypted: usize,
    directory: usize,
    no_unnamed_data: usize,
    other_readable: usize,
    open_failure: usize,
}

fn classify_unknown_sample(sample: &[(u64, bool)]) -> UnknownBuckets {
    let volume_name: Vec<u16> = r"\\.\C:".encode_utf16().chain(std::iter::once(0)).collect();
    let volume = unsafe {
        CreateFileW(
            volume_name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(volume, INVALID_HANDLE_VALUE, "failed to open volume");
    let mut buckets = UnknownBuckets::default();
    for &(frn, parser_directory) in sample {
        let descriptor = file_id_descriptor(frn);
        let file = unsafe {
            OpenFileById(
                volume,
                &descriptor,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null_mut(),
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        };
        if file == INVALID_HANDLE_VALUE {
            let error = unsafe { GetLastError() };
            if matches!(error, 2 | 3 | 6) {
                buckets.deleted += 1;
            } else {
                buckets.open_failure += 1;
            }
            continue;
        }
        let mut info = FileAttributeTagInfo::default();
        let readable = unsafe {
            GetFileInformationByHandleEx(
                file,
                FILE_ATTRIBUTE_TAG_INFO,
                (&mut info as *mut FileAttributeTagInfo).cast(),
                std::mem::size_of::<FileAttributeTagInfo>() as u32,
            )
        } != 0;
        unsafe { CloseHandle(file) };
        if !readable {
            buckets.open_failure += 1;
        } else if info.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            buckets.reparse += 1;
        } else if info.attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
            buckets.sparse += 1;
        } else if info.attributes & FILE_ATTRIBUTE_ENCRYPTED != 0 {
            buckets.encrypted += 1;
        } else if parser_directory || info.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            buckets.directory += 1;
        } else {
            // The raw parser supplied `size_exact = false`, so a readable,
            // ordinary file reached this bucket because no unnamed data
            // attribute was observed during enumeration.
            buckets.no_unnamed_data += 1;
        }
    }
    unsafe { CloseHandle(volume) };
    buckets
}

fn file_id_descriptor(frn: u64) -> FileIdDescriptor {
    let mut identifier = [0u8; 16];
    identifier[..8].copy_from_slice(&frn.to_le_bytes());
    FileIdDescriptor {
        size: std::mem::size_of::<FileIdDescriptor>() as u32,
        kind: FILE_ID_TYPE,
        identifier,
    }
}

#[test]
fn raw_and_usn_enumeration_agree() {
    let mut raw_frns = HashSet::new();
    let mut sample: HashMap<u64, HashSet<(u64, Vec<u8>)>> = HashMap::new();
    let mut slots = Vec::with_capacity(10_000);
    let mut seen = 0u64;
    let mut random = 0x9e37_79b9_7f4a_7c15u64;
    let mut size_samples = Vec::with_capacity(2_000);
    let mut sizes_seen = 0u64;
    let mut size_random = 0xd1b5_4a32_d192_ed03u64;
    let mut incomplete_metadata = HashSet::new();
    let raw_started = std::time::Instant::now();
    let report = match scry_fsevents::mft::enumerate_mft_raw_with_names(
        "C:",
        |frn, _, _, is_dir, _, size, _size_exact| {
            raw_frns.insert(frn);
            if !is_dir {
                sizes_seen += 1;
                let slot = if size_samples.len() < 2_000 {
                    size_samples.push((frn, size));
                    None
                } else {
                    size_random ^= size_random << 13;
                    size_random ^= size_random >> 7;
                    size_random ^= size_random << 17;
                    let candidate = size_random % sizes_seen;
                    (candidate < 2_000).then_some(candidate as usize)
                };
                if let Some(slot) = slot {
                    size_samples[slot] = (frn, size);
                }
            }
        },
        |frn, names, metadata_complete| {
            if !metadata_complete {
                incomplete_metadata.insert(frn);
            }
            if names.is_empty() {
                return;
            }
            seen += 1;
            let slot = if slots.len() < 10_000 {
                slots.push(frn);
                Some(slots.len() - 1)
            } else {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let candidate = random % seen;
                (candidate < 10_000).then_some(candidate as usize)
            };
            if let Some(slot) = slot {
                if let Some(replaced) = slots.get_mut(slot) {
                    sample.remove(replaced);
                    *replaced = frn;
                }
                sample.insert(
                    frn,
                    names
                        .iter()
                        .map(|name| (name.parent_frn, name.name.as_bytes().to_vec()))
                        .collect(),
                );
            }
        },
    ) {
        Ok(report) => report,
        Err(error) => {
            println!("skipped raw comparison (needs elevation or supported NTFS): {error}");
            return;
        }
    };
    let raw_elapsed = raw_started.elapsed();
    size_samples.retain(|(frn, _)| !incomplete_metadata.contains(frn));
    let sizes_checked = validate_sizes(&size_samples);
    assert_eq!(sizes_checked, 1_000, "could not open 1,000 sampled FRNs");

    let mut usn_frns = HashSet::new();
    let mut mismatches = Vec::new();
    let usn_started = std::time::Instant::now();
    let usn_result = scry_fsevents::enumerate_mft_usn("C:", |frn, parent, name, _, _, _| {
        usn_frns.insert(frn);
        if let Some(valid_names) = sample.get(&frn) {
            if !valid_names.contains(&(parent, name.to_vec())) {
                mismatches.push((frn, valid_names.clone(), parent, name.to_vec()));
            }
        }
    });
    if let Err(error) = usn_result {
        println!("skipped USN comparison: {error}");
        return;
    }
    let usn_elapsed = usn_started.elapsed();

    let count_difference = raw_frns.len().abs_diff(usn_frns.len());
    let allowed_difference = usn_frns.len().div_ceil(100);
    let only_raw = raw_frns.difference(&usn_frns).count();
    let only_usn = usn_frns.difference(&raw_frns).count();
    println!(
        "raw={} in {:?}, usn={} in {:?}, only_raw={}, only_usn={}, sizes_checked={}, report={report:?}",
        raw_frns.len(),
        raw_elapsed,
        usn_frns.len(),
        usn_elapsed,
        only_raw,
        only_usn,
        sizes_checked
    );
    // The former single-name oracle failed on records with 43, 38, and 5 hard
    // links. NTFS and FSCTL_ENUM_USN_DATA may select different valid links, so
    // membership in the complete Win32-name set is the strict identity check.
    assert!(
        count_difference <= allowed_difference,
        "entry counts differ by more than 1%"
    );
    assert!(
        mismatches.is_empty(),
        "name/parent mismatches: {mismatches:?}"
    );
}

fn validate_sizes(samples: &[(u64, u64)]) -> usize {
    let volume_name: Vec<u16> = r"\\.\C:".encode_utf16().chain(std::iter::once(0)).collect();
    let volume = unsafe {
        CreateFileW(
            volume_name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(
        volume, INVALID_HANDLE_VALUE,
        "failed to open volume for size validation"
    );
    let mut checked = 0;
    let mut first_open_error = None;
    for &(frn, expected) in samples {
        let descriptor = FileIdDescriptor {
            size: std::mem::size_of::<FileIdDescriptor>() as u32,
            kind: FILE_ID_TYPE,
            identifier: {
                let mut bytes = [0u8; 16];
                bytes[..8].copy_from_slice(&frn.to_le_bytes());
                bytes
            },
        };
        let file = unsafe {
            OpenFileById(
                volume,
                &descriptor,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null_mut(),
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        };
        if file == INVALID_HANDLE_VALUE {
            first_open_error.get_or_insert_with(|| unsafe { GetLastError() });
            continue;
        }
        let mut actual = 0i64;
        let succeeded = unsafe { GetFileSizeEx(file, &mut actual) } != 0;
        unsafe { CloseHandle(file) };
        if !succeeded || actual < 0 {
            continue;
        }
        assert_eq!(
            scry_core::bytes_to_size_kib(expected),
            scry_core::bytes_to_size_kib(actual as u64),
            "size mismatch for FRN {frn}"
        );
        checked += 1;
        if checked == 1_000 {
            break;
        }
    }
    unsafe { CloseHandle(volume) };
    if checked == 0 {
        eprintln!("OpenFileById failed for every sample; first error={first_open_error:?}");
    }
    checked
}

type Handle = *mut c_void;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
const FILE_SHARE_READ: u32 = 1;
const FILE_SHARE_WRITE: u32 = 2;
const FILE_SHARE_DELETE: u32 = 4;
const OPEN_EXISTING: u32 = 3;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const GENERIC_READ: u32 = 0x8000_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_ID_TYPE: u32 = 0;
const FILE_ATTRIBUTE_TAG_INFO: u32 = 9;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x4000;

#[repr(C, align(8))]
struct FileIdDescriptor {
    size: u32,
    kind: u32,
    identifier: [u8; 16],
}

#[repr(C)]
#[derive(Default)]
struct FileAttributeTagInfo {
    attributes: u32,
    reparse_tag: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut c_void,
        creation_disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn OpenFileById(
        volume: Handle,
        descriptor: *const FileIdDescriptor,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut c_void,
        flags: u32,
    ) -> Handle;
    fn GetFileSizeEx(file: Handle, size: *mut i64) -> i32;
    fn GetFileInformationByHandleEx(file: Handle, class: u32, info: *mut c_void, size: u32) -> i32;
    fn CloseHandle(object: Handle) -> i32;
    fn GetLastError() -> u32;
}
