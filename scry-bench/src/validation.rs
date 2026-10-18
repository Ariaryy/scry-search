use scry_client::ResultEntry;
use sha2::{Digest, Sha256};

/// Fixed-size reference per query step. Do not retain every step's result
/// paths: even at limit 20,000, validation retains only one result page at a
/// time plus these keys. Hashing and comparison happen outside query timers.
#[derive(Clone, PartialEq, Eq)]
pub struct Reference {
    pub count: usize,
    digest: [u8; 32],
}

impl Reference {
    pub fn hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for byte in self.digest {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 15) as usize] as char);
        }
        out
    }
    pub fn of(entries: &[ResultEntry]) -> Self {
        let mut hash = Sha256::new();
        hash.update((entries.len() as u64).to_le_bytes());
        for entry in entries {
            hash.update((entry.path.len() as u64).to_le_bytes());
            hash.update(entry.path.as_bytes());
            hash.update(entry.size.to_le_bytes());
            hash.update(entry.mtime.to_le_bytes());
            hash.update([entry.is_dir as u8, entry.size_exact as u8]);
        }
        Self {
            count: entries.len(),
            digest: hash.finalize().into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_catches_empty_same_count_wrong_identity_order_and_metadata() {
        let a = ResultEntry {
            path: "archive/report.txt".into(),
            size: 12,
            mtime: 3,
            is_dir: false,
            size_exact: true,
        };
        let b = ResultEntry {
            path: "archive/summary.txt".into(),
            ..a.clone()
        };
        assert!(Reference::of(std::slice::from_ref(&a)) != Reference::of(&[]));
        assert!(Reference::of(std::slice::from_ref(&a)) != Reference::of(std::slice::from_ref(&b)));
        assert!(Reference::of(&[a.clone(), b.clone()]) != Reference::of(&[b, a.clone()]));
        for changed in [
            ResultEntry {
                size: 13,
                ..a.clone()
            },
            ResultEntry {
                mtime: 4,
                ..a.clone()
            },
            ResultEntry {
                is_dir: true,
                ..a.clone()
            },
            ResultEntry {
                size_exact: false,
                ..a.clone()
            },
        ] {
            assert!(Reference::of(std::slice::from_ref(&a)) != Reference::of(&[changed]));
        }
    }
}
