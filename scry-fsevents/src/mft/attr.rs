use super::record::{read_u16, read_u32, read_u64, AttributeRef};
use super::MftError;

pub const STANDARD_INFORMATION: u32 = 0x10;
pub const ATTRIBUTE_LIST: u32 = 0x20;
pub const FILE_NAME: u32 = 0x30;
pub const DATA: u32 = 0x80;

#[derive(Debug, PartialEq, Eq)]
pub struct FileNameInfo {
    pub parent_frn: u64,
    pub namespace: u8,
    pub name: String,
}

pub fn resident_value<'a>(attribute: &AttributeRef<'a>) -> Result<&'a [u8], MftError> {
    if attribute.non_resident {
        return Err(MftError::Invalid(
            "resident value requested from non-resident attribute",
        ));
    }
    let length = read_u32(attribute.bytes, 0x10)? as usize;
    let offset = read_u16(attribute.bytes, 0x14)? as usize;
    let end = offset
        .checked_add(length)
        .ok_or(MftError::Invalid("resident value overflow"))?;
    attribute
        .bytes
        .get(offset..end)
        .ok_or(MftError::Invalid("resident value exceeds attribute"))
}

pub fn parse_file_name(attribute: &AttributeRef<'_>) -> Result<FileNameInfo, MftError> {
    if attribute.type_code != FILE_NAME {
        return Err(MftError::Invalid("not a FILE_NAME attribute"));
    }
    let value = resident_value(attribute)?;
    let parent_frn = read_u64(value, 0)?;
    let code_units = *value
        .get(0x40)
        .ok_or(MftError::Invalid("truncated FILE_NAME length"))? as usize;
    let namespace = *value
        .get(0x41)
        .ok_or(MftError::Invalid("truncated FILE_NAME namespace"))?;
    let byte_length = code_units
        .checked_mul(2)
        .ok_or(MftError::Invalid("FILE_NAME length overflow"))?;
    let end = 0x42usize
        .checked_add(byte_length)
        .ok_or(MftError::Invalid("FILE_NAME end overflow"))?;
    let name_bytes = value
        .get(0x42..end)
        .ok_or(MftError::Invalid("truncated FILE_NAME value"))?;
    let utf16: Vec<u16> = name_bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Ok(FileNameInfo {
        parent_frn,
        namespace,
        name: String::from_utf16_lossy(&utf16),
    })
}

pub fn parse_standard_information(attribute: &AttributeRef<'_>) -> Result<u32, MftError> {
    if attribute.type_code != STANDARD_INFORMATION {
        return Err(MftError::Invalid("not a STANDARD_INFORMATION attribute"));
    }
    let value = resident_value(attribute)?;
    let ticks = read_u64(value, 8)?;
    let signed = i64::try_from(ticks).map_err(|_| MftError::Invalid("FILETIME exceeds i64"))?;
    Ok(scry_core::filetime_to_secs(signed))
}

pub fn data_size(attribute: &AttributeRef<'_>) -> Result<u64, MftError> {
    if attribute.type_code != DATA {
        return Err(MftError::Invalid("not a DATA attribute"));
    }
    let name_length = *attribute
        .bytes
        .get(9)
        .ok_or(MftError::Invalid("truncated DATA header"))?;
    if name_length != 0 {
        return Err(MftError::Invalid("named DATA stream is out of scope"));
    }
    if attribute.non_resident {
        read_u64(attribute.bytes, 0x30)
    } else {
        Ok(read_u32(attribute.bytes, 0x10)? as u64)
    }
}

pub fn has_attribute_list(attribute: &AttributeRef<'_>) -> bool {
    attribute.type_code == ATTRIBUTE_LIST
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resident(type_code: u32, value: &[u8]) -> Vec<u8> {
        let length = (0x18 + value.len()).next_multiple_of(8);
        let mut attribute = vec![0u8; length];
        attribute[0..4].copy_from_slice(&type_code.to_le_bytes());
        attribute[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        attribute[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        attribute[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        attribute[0x18..0x18 + value.len()].copy_from_slice(value);
        attribute
    }

    fn reference(bytes: &[u8]) -> AttributeRef<'_> {
        AttributeRef {
            type_code: read_u32(bytes, 0).unwrap(),
            non_resident: bytes[8] != 0,
            bytes,
        }
    }

    #[test]
    fn file_name_parses_unicode_and_code_units() {
        let name: Vec<u16> = "rocket_🚀.txt".encode_utf16().collect();
        let mut value = vec![0u8; 0x42 + name.len() * 2];
        value[0..8].copy_from_slice(&0x0007_0000_0000_1234u64.to_le_bytes());
        value[0x40] = name.len() as u8;
        value[0x41] = 1;
        for (slot, unit) in value[0x42..].chunks_exact_mut(2).zip(name) {
            slot.copy_from_slice(&unit.to_le_bytes());
        }
        let attribute = resident(FILE_NAME, &value);
        let parsed = parse_file_name(&reference(&attribute)).unwrap();
        assert_eq!(parsed.parent_frn, 0x0007_0000_0000_1234);
        assert_eq!(parsed.name, "rocket_🚀.txt");
        assert_eq!(parsed.namespace, 1);
    }

    #[test]
    fn file_name_dos_namespace_is_identified() {
        let mut value = vec![0u8; 0x42];
        value[0x41] = 2;
        let attribute = resident(FILE_NAME, &value);
        assert_eq!(
            parse_file_name(&reference(&attribute)).unwrap().namespace,
            2
        );
    }

    #[test]
    fn standard_information_filetime_converts() {
        let mut value = vec![0u8; 16];
        value[8..16].copy_from_slice(&126_227_808_000_000_000u64.to_le_bytes());
        let attribute = resident(STANDARD_INFORMATION, &value);
        assert_eq!(
            parse_standard_information(&reference(&attribute)).unwrap(),
            978_307_200
        );
    }

    #[test]
    fn data_size_resident_nonresident_and_directory_default() {
        let resident_data = resident(DATA, &[0u8; 37]);
        assert_eq!(data_size(&reference(&resident_data)).unwrap(), 37);

        let mut nonresident = vec![0u8; 0x40];
        nonresident[0..4].copy_from_slice(&DATA.to_le_bytes());
        nonresident[4..8].copy_from_slice(&0x40u32.to_le_bytes());
        nonresident[8] = 1;
        nonresident[0x30..0x38].copy_from_slice(&9_999u64.to_le_bytes());
        assert_eq!(data_size(&reference(&nonresident)).unwrap(), 9_999);
        let directory_size = 0u64;
        assert_eq!(directory_size, 0);
    }

    #[test]
    fn attribute_list_is_detected() {
        let attribute = resident(ATTRIBUTE_LIST, &[]);
        assert!(has_attribute_list(&reference(&attribute)));
    }
}
