use super::MftError;

const FILE_MAGIC: &[u8; 4] = b"FILE";
const BAAD_MAGIC: &[u8; 4] = b"BAAD";
const ATTRIBUTE_END: u32 = 0xffff_ffff;

pub struct ParsedRecord<'a> {
    bytes: &'a [u8],
    first_attribute: usize,
    used_size: usize,
    flags: u16,
    base_reference: u64,
    record_number: u32,
    sequence_number: u16,
}

impl<'a> ParsedRecord<'a> {
    pub fn parse(bytes: &'a mut [u8], sector_size: usize) -> Result<Option<Self>, MftError> {
        let magic = bytes
            .get(0..4)
            .ok_or(MftError::Invalid("truncated FILE magic"))?;
        if magic == BAAD_MAGIC || magic != FILE_MAGIC {
            return Ok(None);
        }
        apply_fixups(bytes, sector_size)?;
        let first_attribute = read_u16(bytes, 0x14)? as usize;
        let flags = read_u16(bytes, 0x16)?;
        let used_size = read_u32(bytes, 0x18)? as usize;
        let base_reference = read_u64(bytes, 0x20)?;
        let record_number = read_u32(bytes, 0x2c)?;
        let sequence_number = read_u16(bytes, 0x10)?;
        if used_size > bytes.len() || first_attribute < 0x30 || first_attribute > used_size {
            return Err(MftError::Invalid("invalid FILE record bounds"));
        }
        Ok(Some(Self {
            bytes,
            first_attribute,
            used_size,
            flags,
            base_reference,
            record_number,
            sequence_number,
        }))
    }

    pub fn attributes(&self) -> AttributeIter<'a> {
        AttributeIter {
            bytes: self.bytes,
            position: self.first_attribute,
            end: self.used_size,
            done: false,
        }
    }

    pub fn in_use(&self) -> bool {
        self.flags & 1 != 0
    }

    pub fn is_dir(&self) -> bool {
        self.flags & 2 != 0
    }

    pub fn is_extension(&self) -> bool {
        self.base_reference != 0
    }

    pub fn record_number(&self) -> u32 {
        self.record_number
    }

    pub fn frn(&self) -> u64 {
        (self.sequence_number as u64) << 48 | self.record_number as u64
    }
}

#[derive(Clone, Copy)]
pub struct AttributeRef<'a> {
    pub type_code: u32,
    pub non_resident: bool,
    pub bytes: &'a [u8],
}

pub struct AttributeIter<'a> {
    bytes: &'a [u8],
    position: usize,
    end: usize,
    done: bool,
}

impl<'a> Iterator for AttributeIter<'a> {
    type Item = Result<AttributeRef<'a>, MftError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.position >= self.end {
            return None;
        }
        let type_code = match read_u32(self.bytes, self.position) {
            Ok(value) => value,
            Err(error) => return self.fail(error),
        };
        if type_code == ATTRIBUTE_END {
            self.done = true;
            return None;
        }
        let length = match read_u32(self.bytes, self.position + 4) {
            Ok(value) => value as usize,
            Err(error) => return self.fail(error),
        };
        if length == 0 || length % 8 != 0 {
            return self.fail(MftError::Invalid("invalid attribute length"));
        }
        let next = match self.position.checked_add(length) {
            Some(value) if value <= self.end => value,
            _ => return self.fail(MftError::Invalid("attribute exceeds FILE record")),
        };
        let bytes = match self.bytes.get(self.position..next) {
            Some(value) => value,
            None => return self.fail(MftError::Invalid("attribute slice out of bounds")),
        };
        let non_resident = match bytes.get(8) {
            Some(value) => *value != 0,
            None => return self.fail(MftError::Invalid("truncated attribute header")),
        };
        self.position = next;
        Some(Ok(AttributeRef {
            type_code,
            non_resident,
            bytes,
        }))
    }
}

impl<'a> AttributeIter<'a> {
    fn fail(&mut self, error: MftError) -> Option<Result<AttributeRef<'a>, MftError>> {
        self.done = true;
        Some(Err(error))
    }
}

fn apply_fixups(bytes: &mut [u8], sector_size: usize) -> Result<(), MftError> {
    if sector_size < 2 || bytes.len() < sector_size || !bytes.len().is_multiple_of(sector_size) {
        return Err(MftError::Invalid("invalid fixup sector geometry"));
    }
    let usa_offset = read_u16(bytes, 4)? as usize;
    let usa_count = read_u16(bytes, 6)? as usize;
    let expected_count = bytes.len() / sector_size + 1;
    if usa_count != expected_count {
        return Err(MftError::Invalid("invalid update sequence count"));
    }
    let usa_bytes = usa_count
        .checked_mul(2)
        .and_then(|size| usa_offset.checked_add(size))
        .ok_or(MftError::Invalid("update sequence bounds overflow"))?;
    if usa_bytes > bytes.len() {
        return Err(MftError::Invalid("update sequence exceeds record"));
    }
    let sequence = [
        *bytes
            .get(usa_offset)
            .ok_or(MftError::Invalid("missing update sequence"))?,
        *bytes
            .get(usa_offset + 1)
            .ok_or(MftError::Invalid("missing update sequence"))?,
    ];
    for sector in 0..(usa_count - 1) {
        let end = (sector + 1)
            .checked_mul(sector_size)
            .ok_or(MftError::Invalid("sector end overflow"))?;
        let trailer = bytes
            .get(end - 2..end)
            .ok_or(MftError::Invalid("sector trailer out of bounds"))?;
        if trailer != sequence {
            return Err(MftError::TornRecord);
        }
        let replacement_offset = usa_offset + (sector + 1) * 2;
        let replacement_raw = bytes
            .get(replacement_offset..replacement_offset + 2)
            .ok_or(MftError::Invalid("fixup replacement out of bounds"))?;
        let replacement = [replacement_raw[0], replacement_raw[1]];
        let destination = bytes
            .get_mut(end - 2..end)
            .ok_or(MftError::Invalid("fixup destination out of bounds"))?;
        destination.copy_from_slice(&replacement);
    }
    Ok(())
}

pub(crate) fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, MftError> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or(MftError::Invalid("truncated u16"))?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

pub(crate) fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, MftError> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or(MftError::Invalid("truncated u32"))?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

pub(crate) fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, MftError> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or(MftError::Invalid("truncated u64"))?;
    Ok(u64::from_le_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_record() -> Vec<u8> {
        let mut record = vec![0u8; 1024];
        record[0..4].copy_from_slice(b"FILE");
        record[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        record[6..8].copy_from_slice(&3u16.to_le_bytes());
        record[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        record[0x16..0x18].copy_from_slice(&1u16.to_le_bytes());
        record[0x18..0x1c].copy_from_slice(&0x48u32.to_le_bytes());
        record[0x2c..0x30].copy_from_slice(&7u32.to_le_bytes());
        record[0x30..0x32].copy_from_slice(&[0xaa, 0xbb]);
        record[0x32..0x34].copy_from_slice(&[0x11, 0x22]);
        record[0x34..0x36].copy_from_slice(&[0x33, 0x44]);
        record[510..512].copy_from_slice(&[0xaa, 0xbb]);
        record[1022..1024].copy_from_slice(&[0xaa, 0xbb]);
        record[0x38..0x3c].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        record
    }

    #[test]
    fn torn_record_is_rejected() {
        let mut record = synthetic_record();
        record[510] ^= 1;
        assert!(ParsedRecord::parse(&mut record, 512).is_err());
    }

    #[test]
    fn attribute_iterator_rejects_zero_length() {
        let mut record = synthetic_record();
        record[0x38..0x3c].copy_from_slice(&0x10u32.to_le_bytes());
        record[0x3c..0x40].copy_from_slice(&0u32.to_le_bytes());
        let parsed = ParsedRecord::parse(&mut record, 512).unwrap().unwrap();
        assert!(parsed.attributes().next().unwrap().is_err());
    }

    #[test]
    fn fixups_are_applied_and_end_marker_stops_iteration() {
        let mut record = synthetic_record();
        let parsed = ParsedRecord::parse(&mut record, 512).unwrap().unwrap();
        assert_eq!(&parsed.bytes[510..512], &[0x11, 0x22]);
        assert_eq!(&parsed.bytes[1022..1024], &[0x33, 0x44]);
        assert_eq!(parsed.record_number(), 7);
        assert!(parsed.in_use());
        assert!(parsed.attributes().next().is_none());
    }

    #[test]
    fn baad_and_invalid_attribute_offsets_are_handled() {
        let mut record = synthetic_record();
        record[0..4].copy_from_slice(b"BAAD");
        assert!(ParsedRecord::parse(&mut record, 512).unwrap().is_none());

        let mut record = synthetic_record();
        record[0x14..0x16].copy_from_slice(&0x500u16.to_le_bytes());
        assert!(ParsedRecord::parse(&mut record, 512).is_err());
    }

    #[test]
    fn parser_never_panics_on_mutated_records() {
        let original = synthetic_record();
        for seed in 0..200_000u32 {
            let mut record = original.clone();
            let mut state = seed.wrapping_add(1);
            for _ in 0..6 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let position = state as usize % record.len();
                record[position] ^= 1 << ((state >> 16) & 7);
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(Some(parsed)) = ParsedRecord::parse(&mut record, 512) {
                    for attribute in parsed.attributes() {
                        if attribute.is_err() {
                            break;
                        }
                    }
                }
            }));
            assert!(result.is_ok(), "parser panicked for mutation seed {seed}");
        }

        let adversarial = [
            (4usize, 0xffu8), // USA offset/count corruption
            (6, 0xff),
            (0x14, 0xff), // first attribute beyond the record
            (0x3c, 0),    // zero attribute length
            (0x38, 0x30), // FILE_NAME type with truncated value
        ];
        for (position, value) in adversarial {
            let mut record = original.clone();
            record[position] = value;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(Some(parsed)) = ParsedRecord::parse(&mut record, 512) {
                    let _ = parsed.attributes().collect::<Vec<_>>();
                }
            }));
            assert!(
                result.is_ok(),
                "parser panicked for adversarial byte {position:#x}"
            );
        }

        for runlist in [
            &[0x88, 0][..],
            &[0x11][..],
            &[0x10, 1, 0][..],
            &[0x91, 1, 0][..],
        ] {
            let result = std::panic::catch_unwind(|| super::super::runlist::decode_runs(runlist));
            assert!(result.is_ok());
        }
    }
}
