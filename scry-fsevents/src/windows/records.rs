//! Shared decoding of packed USN records and their filenames.

use super::ffi;

/// Walks the USN_RECORD_V2 entries in an ioctl output buffer, handing each
/// header and its raw UTF-16LE filename bytes to `f` without allocating.
///
/// The previous signature returned `Vec<(Header, String)>`, which allocated a
/// vector and a `String` per record per 64 KiB buffer — tens of millions of
/// transient allocations across a full MFT enumeration.
///
/// Both `FSCTL_ENUM_USN_DATA` and `FSCTL_READ_USN_JOURNAL` share this exact
/// wire format (an 8-byte cursor followed by packed variable-length records),
/// so enumeration and live-journal reads both use this.
pub(super) fn for_each_usn_record(
    buf: &[u8],
    bytes_returned: usize,
    mut f: impl FnMut(&ffi::UsnRecordV2Header, &[u8]),
) {
    let mut offset = 8usize;
    while offset + std::mem::size_of::<ffi::UsnRecordV2Header>() <= bytes_returned {
        let header: ffi::UsnRecordV2Header =
            unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr() as *const _) };
        if header.record_length == 0 {
            break;
        }
        // A malformed record_length smaller than the header itself would
        // make `offset` fail to advance and spin forever.
        if (header.record_length as usize) < std::mem::size_of::<ffi::UsnRecordV2Header>() {
            break;
        }

        // `buf` is the full ioctl allocation, not the valid region — the
        // bound must be `bytes_returned`, not `buf.len()`. Checking against
        // `buf.len()` let a truncated final record read stale bytes left
        // over from a previous call and yield a garbage filename instead of
        // being skipped.
        let record_end = offset + header.record_length as usize;
        if record_end > bytes_returned {
            break;
        }
        let name_start = offset + header.file_name_offset as usize;
        let name_end = name_start + header.file_name_length as usize;
        if name_end > record_end {
            break;
        }

        f(&header, &buf[name_start..name_end]);
        offset = record_end;
    }
}

/// Decodes UTF-16LE name bytes into `out` (cleared first) as UTF-8, replacing
/// unpaired surrogates. NTFS filenames are UCS-2 and may contain unpaired
/// surrogates, so lossy conversion is correct, not a shortcut.
pub(super) fn decode_name_into(name_bytes: &[u8], out: &mut String) {
    out.clear();
    let utf16: Vec<u16> = name_bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_ne_bytes(c))
        .collect();
    out.push_str(&String::from_utf16_lossy(&utf16));
}
