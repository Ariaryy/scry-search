use super::MftError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub lcn: Option<i64>,
    pub cluster_count: u64,
}

pub fn decode_runs(bytes: &[u8]) -> Result<Vec<Run>, MftError> {
    let mut runs = Vec::new();
    let mut position = 0usize;
    let mut previous_lcn = 0i64;
    loop {
        let header = *bytes
            .get(position)
            .ok_or(MftError::Invalid("unterminated run list"))?;
        position = position
            .checked_add(1)
            .ok_or(MftError::Invalid("run-list position overflow"))?;
        if header == 0 {
            return Ok(runs);
        }
        let length_size = (header & 0x0f) as usize;
        let offset_size = (header >> 4) as usize;
        if length_size == 0 || length_size > 8 || offset_size > 8 {
            return Err(MftError::Invalid("invalid run-list field width"));
        }
        let length_end = position
            .checked_add(length_size)
            .ok_or(MftError::Invalid("run length offset overflow"))?;
        let length_bytes = bytes
            .get(position..length_end)
            .ok_or(MftError::Invalid("truncated run length"))?;
        let cluster_count = unsigned_le(length_bytes);
        if cluster_count == 0 {
            return Err(MftError::Invalid("zero-length data run"));
        }
        position = length_end;

        let lcn = if offset_size == 0 {
            None
        } else {
            let offset_end = position
                .checked_add(offset_size)
                .ok_or(MftError::Invalid("run offset overflow"))?;
            let offset_bytes = bytes
                .get(position..offset_end)
                .ok_or(MftError::Invalid("truncated run offset"))?;
            let delta = signed_le(offset_bytes);
            previous_lcn = previous_lcn
                .checked_add(delta)
                .ok_or(MftError::Invalid("LCN delta overflow"))?;
            if previous_lcn < 0 {
                return Err(MftError::Invalid("negative absolute LCN"));
            }
            position = offset_end;
            Some(previous_lcn)
        };
        runs.push(Run { lcn, cluster_count });
    }
}

fn unsigned_le(bytes: &[u8]) -> u64 {
    bytes.iter().enumerate().fold(0u64, |value, (shift, byte)| {
        value | (*byte as u64) << (shift * 8)
    })
}

fn signed_le(bytes: &[u8]) -> i64 {
    let value = unsigned_le(bytes);
    let bits = bytes.len() * 8;
    if bits < 64 && value & (1u64 << (bits - 1)) != 0 {
        (value | (!0u64 << bits)) as i64
    } else {
        value as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runlist_decodes_single_and_sparse_runs() {
        assert_eq!(
            decode_runs(&[0x11, 0x18, 0x34, 0x00]).unwrap(),
            vec![Run {
                lcn: Some(0x34),
                cluster_count: 0x18
            }]
        );
        assert_eq!(
            decode_runs(&[0x01, 0x07, 0x00]).unwrap(),
            vec![Run {
                lcn: None,
                cluster_count: 7
            }]
        );
    }

    #[test]
    fn runlist_decodes_negative_delta() {
        // First LCN 0x100, then delta -52 (0xffcc), yielding LCN 0xcc.
        let runs = decode_runs(&[0x21, 0x18, 0x00, 0x01, 0x21, 0x42, 0xcc, 0xff, 0x00]).unwrap();
        assert_eq!(runs[0].lcn, Some(0x100));
        assert_eq!(runs[1].lcn, Some(0xcc));
        assert_eq!(runs[1].cluster_count, 0x42);
    }

    #[test]
    fn runlist_rejects_truncated_and_invalid_fields() {
        for bytes in [
            &[0x11, 0x18][..],
            &[0x19, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0][..],
            &[0x91, 1, 0][..],
            &[0x10, 1, 0][..],
        ] {
            assert!(decode_runs(bytes).is_err());
        }
    }
}
