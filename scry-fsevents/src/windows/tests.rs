use super::journal::*;
use super::records::*;
use super::*;

fn cursor(journal_id: u64, first_usn: i64, next_usn: i64, volume_serial: u64) -> JournalCursor {
    JournalCursor {
        journal_id,
        first_usn,
        next_usn,
        volume_serial,
    }
}

#[test]
fn replay_cursor_rejects_a_changed_journal_or_volume() {
    let current = cursor(2, 100, 200, 3);
    assert!(matches!(
        validate_replay_cursor(cursor(1, 0, 150, 3), current),
        Err(WindowsBackendError::JournalChanged)
    ));
    assert!(matches!(
        validate_replay_cursor(cursor(2, 0, 150, 4), current),
        Err(WindowsBackendError::JournalChanged)
    ));
}

#[test]
fn replay_cursor_rejects_an_aged_out_position() {
    assert!(matches!(
        validate_replay_cursor(cursor(2, 0, 99, 3), cursor(2, 100, 200, 3)),
        Err(WindowsBackendError::JournalAgedOut)
    ));
}

#[test]
fn replay_cursor_accepts_an_available_position() {
    assert!(validate_replay_cursor(cursor(2, 0, 150, 3), cursor(2, 100, 200, 3)).is_ok());
}

fn header(reason: u32, source_info: u32) -> ffi::UsnRecordV2Header {
    ffi::UsnRecordV2Header {
        record_length: 0,
        major_version: 2,
        minor_version: 0,
        file_reference_number: 1,
        parent_file_reference_number: 2,
        usn: 0,
        time_stamp: 0,
        reason,
        source_info,
        security_id: 0,
        file_attributes: 0,
        file_name_length: 0,
        file_name_offset: 0,
    }
}

#[test]
fn classify_marks_auxiliary_source() {
    let h = header(ffi::USN_REASON_FILE_CREATE, ffi::USN_SOURCE_AUXILIARY_DATA);
    let ChangeEvent::Created { is_auxiliary, .. } = classify(&h, "foo".to_string()) else {
        panic!("expected Created");
    };
    assert!(is_auxiliary);

    let h = header(ffi::USN_REASON_FILE_CREATE, 0);
    let ChangeEvent::Created { is_auxiliary, .. } = classify(&h, "foo".to_string()) else {
        panic!("expected Created");
    };
    assert!(!is_auxiliary);
}

const HEADER_SIZE: usize = std::mem::size_of::<ffi::UsnRecordV2Header>();

/// Appends one well-formed USN_RECORD_V2 record (header + UTF-16LE name)
/// to `buf` and returns the record's total byte length.
fn append_record(buf: &mut Vec<u8>, name: &str) -> usize {
    let name_utf16: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let record_len = HEADER_SIZE + name_utf16.len();
    let h = ffi::UsnRecordV2Header {
        record_length: record_len as u32,
        major_version: 2,
        minor_version: 0,
        file_reference_number: 1,
        parent_file_reference_number: 2,
        usn: 0,
        time_stamp: 0,
        reason: ffi::USN_REASON_FILE_CREATE,
        source_info: 0,
        security_id: 0,
        file_attributes: 0,
        file_name_length: name_utf16.len() as u16,
        file_name_offset: HEADER_SIZE as u16,
    };
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            (&h as *const ffi::UsnRecordV2Header) as *const u8,
            HEADER_SIZE,
        )
    };
    buf.extend_from_slice(header_bytes);
    buf.extend_from_slice(&name_utf16);
    record_len
}

#[test]
fn truncated_trailing_record_is_skipped_not_misread() {
    let mut buf = vec![0u8; 8]; // 8-byte resume cursor
    append_record(&mut buf, "valid.txt");

    // A second header claiming a record_length that extends past the
    // buffer's valid region (bytes_returned) — must be skipped, not read.
    let bad_start = buf.len();
    let bad = ffi::UsnRecordV2Header {
        record_length: 4096,
        major_version: 2,
        minor_version: 0,
        file_reference_number: 3,
        parent_file_reference_number: 2,
        usn: 0,
        time_stamp: 0,
        reason: ffi::USN_REASON_FILE_CREATE,
        source_info: 0,
        security_id: 0,
        file_attributes: 0,
        file_name_length: 8,
        file_name_offset: HEADER_SIZE as u16,
    };
    let bad_bytes = unsafe {
        std::slice::from_raw_parts(
            (&bad as *const ffi::UsnRecordV2Header) as *const u8,
            HEADER_SIZE,
        )
    };
    buf.extend_from_slice(bad_bytes);
    // Pad so the buffer is real allocated memory beyond bytes_returned,
    // simulating stale bytes left over from a previous ioctl call.
    buf.extend_from_slice(&[0xAAu8; 64]);
    let bytes_returned = bad_start + HEADER_SIZE; // truncated: no room for the name

    let mut calls = 0;
    for_each_usn_record(&buf, bytes_returned, |_h, _name| {
        calls += 1;
    });
    assert_eq!(
        calls, 1,
        "only the first, well-formed record should be visited"
    );
}

#[test]
fn zero_or_undersized_record_length_terminates() {
    let mut buf = vec![0u8; 8];
    let bad = ffi::UsnRecordV2Header {
        record_length: 4,
        major_version: 2,
        minor_version: 0,
        file_reference_number: 1,
        parent_file_reference_number: 2,
        usn: 0,
        time_stamp: 0,
        reason: ffi::USN_REASON_FILE_CREATE,
        source_info: 0,
        security_id: 0,
        file_attributes: 0,
        file_name_length: 0,
        file_name_offset: 0,
    };
    let bad_bytes = unsafe {
        std::slice::from_raw_parts(
            (&bad as *const ffi::UsnRecordV2Header) as *const u8,
            HEADER_SIZE,
        )
    };
    buf.extend_from_slice(bad_bytes);
    buf.extend_from_slice(&[0u8; 256]);
    let bytes_returned = buf.len();

    let mut calls = 0;
    for_each_usn_record(&buf, bytes_returned, |_h, _name| {
        calls += 1;
    });
    assert_eq!(
        calls, 0,
        "an undersized record_length must not spin forever"
    );
}

#[test]
fn decode_name_into_handles_unpaired_surrogates() {
    // NTFS names are UCS-2 and may contain an unpaired surrogate, which
    // is not valid UTF-16. Lossy decoding must not panic.
    let mut valid: Vec<u16> = "readme".encode_utf16().collect();
    valid.push(0xD800); // unpaired high surrogate, no low surrogate follows
    let bytes: Vec<u8> = valid.iter().flat_map(|u| u.to_le_bytes()).collect();

    let mut out = String::new();
    decode_name_into(&bytes, &mut out);
    assert!(out.starts_with("readme"));
    assert!(
        out.contains('\u{FFFD}'),
        "unpaired surrogate should become U+FFFD"
    );
}
