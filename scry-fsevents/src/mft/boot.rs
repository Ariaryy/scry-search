use std::io::Read;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;

use super::MftError;

const FILE_SHARE_READ: u32 = 1;
const FILE_SHARE_WRITE: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeGeometry {
    pub bytes_per_sector: u16,
    pub cluster_size: u32,
    pub file_record_size: u32,
    pub mft_start_lcn: u64,
    pub physical_sector_size: u32,
}

impl VolumeGeometry {
    pub fn mft_offset(self) -> Option<u64> {
        self.mft_start_lcn.checked_mul(self.cluster_size as u64)
    }
}

pub fn parse_boot_sector(bytes: &[u8]) -> Result<VolumeGeometry, MftError> {
    if bytes.get(3..11) != Some(b"NTFS    ".as_slice()) {
        return Err(MftError::Invalid("OEM identifier is not NTFS"));
    }
    let bytes_per_sector = read_u16(bytes, 0x0b)?;
    if !(512..=4096).contains(&bytes_per_sector) || !bytes_per_sector.is_power_of_two() {
        return Err(MftError::Invalid("invalid logical sector size"));
    }
    let sectors_per_cluster = *bytes
        .get(0x0d)
        .ok_or(MftError::Invalid("missing cluster geometry"))?;
    let cluster_size = scaled_size(sectors_per_cluster, bytes_per_sector as u32)?;
    let record_scale = *bytes
        .get(0x40)
        .ok_or(MftError::Invalid("missing file record geometry"))?;
    let file_record_size = scaled_size(record_scale, cluster_size)?;
    if !(512..=4096).contains(&file_record_size) || !file_record_size.is_power_of_two() {
        return Err(MftError::Invalid("invalid file record size"));
    }
    let mft_start_lcn = read_u64(bytes, 0x30)?;
    mft_start_lcn
        .checked_mul(cluster_size as u64)
        .ok_or(MftError::Invalid("MFT offset overflow"))?;
    Ok(VolumeGeometry {
        bytes_per_sector,
        cluster_size,
        file_record_size,
        mft_start_lcn,
        physical_sector_size: 4096,
    })
}

pub fn read_volume_boot(volume: &str) -> Result<VolumeGeometry, MftError> {
    let path = format!(r"\\.\{volume}");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)?;
    let mut bytes = [0u8; 4096];
    file.read_exact(&mut bytes)?;
    let mut geometry = parse_boot_sector(&bytes)?;
    geometry.physical_sector_size = physical_sector_size(&file).unwrap_or(4096);
    Ok(geometry)
}

fn physical_sector_size(file: &std::fs::File) -> Option<u32> {
    let query = StoragePropertyQuery {
        property_id: 6,
        query_type: 0,
        additional_parameters: [0],
    };
    let mut descriptor = StorageAccessAlignmentDescriptor::default();
    let mut returned = 0u32;
    let success = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&query as *const StoragePropertyQuery).cast(),
            std::mem::size_of::<StoragePropertyQuery>() as u32,
            (&mut descriptor as *mut StorageAccessAlignmentDescriptor).cast(),
            std::mem::size_of::<StorageAccessAlignmentDescriptor>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    (success != 0 && descriptor.bytes_per_physical_sector.is_power_of_two())
        .then_some(descriptor.bytes_per_physical_sector)
}

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002d_1400;

#[repr(C)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional_parameters: [u8; 1],
}

#[repr(C)]
#[derive(Default)]
struct StorageAccessAlignmentDescriptor {
    version: u32,
    size: u32,
    bytes_per_cache_line: u32,
    bytes_offset_for_cache_alignment: u32,
    bytes_per_logical_sector: u32,
    bytes_per_physical_sector: u32,
    bytes_offset_for_sector_alignment: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn DeviceIoControl(
        device: std::os::windows::io::RawHandle,
        control_code: u32,
        input: *const std::ffi::c_void,
        input_size: u32,
        output: *mut std::ffi::c_void,
        output_size: u32,
        bytes_returned: *mut u32,
        overlapped: *mut std::ffi::c_void,
    ) -> i32;
}

fn scaled_size(encoded: u8, unit: u32) -> Result<u32, MftError> {
    let signed = encoded as i8;
    if signed < 0 {
        1u32.checked_shl((-signed) as u32)
            .ok_or(MftError::Invalid("encoded geometry shift overflow"))
    } else if signed == 0 {
        Err(MftError::Invalid("zero geometry multiplier"))
    } else {
        unit.checked_mul(signed as u32)
            .ok_or(MftError::Invalid("geometry multiplication overflow"))
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, MftError> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or(MftError::Invalid("truncated u16"))?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, MftError> {
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

    fn known_good() -> [u8; 512] {
        let mut sector = [0u8; 512];
        sector[3..11].copy_from_slice(b"NTFS    ");
        sector[0x0b..0x0d].copy_from_slice(&512u16.to_le_bytes());
        sector[0x0d] = 8; // 4096-byte clusters
        sector[0x30..0x38].copy_from_slice(&786_432u64.to_le_bytes());
        sector[0x40] = 0xf6; // 2^10 = 1024-byte records
        sector
    }

    #[test]
    fn boot_sector_parses_known_good_bytes() {
        let geometry = parse_boot_sector(&known_good()).unwrap();
        assert_eq!(geometry.bytes_per_sector, 512);
        assert_eq!(geometry.cluster_size, 4096);
        assert_eq!(geometry.file_record_size, 1024);
        assert_eq!(geometry.mft_start_lcn, 786_432);
        assert_eq!(geometry.mft_offset(), Some(3_221_225_472));
    }

    #[test]
    fn boot_sector_rejects_bad_identifiers_and_sector_sizes() {
        let mut sector = known_good();
        sector[3] = b'X';
        assert!(parse_boot_sector(&sector).is_err());
        for size in [0u16, 3, u16::MAX] {
            let mut sector = known_good();
            sector[0x0b..0x0d].copy_from_slice(&size.to_le_bytes());
            assert!(parse_boot_sector(&sector).is_err());
        }
    }

    #[test]
    fn sectors_per_cluster_signed_shift() {
        let mut sector = known_good();
        sector[0x0d] = 0xf6;
        assert_eq!(parse_boot_sector(&sector).unwrap().cluster_size, 1024);
    }

    #[test]
    #[ignore = "requires an elevated NTFS volume"]
    fn boot_sector_of_real_volume() {
        match read_volume_boot("C:") {
            Ok(geometry) => println!("{geometry:?}"),
            Err(error) => println!("skipped: {error}"),
        }
    }
}
