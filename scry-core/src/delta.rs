use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct Bitset {
    words: Vec<u64>,
    ones: u32,
}

impl Bitset {
    pub fn new(bits: usize) -> Self {
        Self {
            words: vec![0; bits.div_ceil(64)],
            ones: 0,
        }
    }

    pub fn set(&mut self, index: u32) -> bool {
        let word = &mut self.words[index as usize / 64];
        let mask = 1u64 << (index % 64);
        if *word & mask != 0 {
            return false;
        }
        *word |= mask;
        self.ones += 1;
        true
    }

    pub fn get(&self, index: u32) -> bool {
        self.words
            .get(index as usize / 64)
            .is_some_and(|word| word & (1u64 << (index % 64)) != 0)
    }

    pub fn count_ones(&self) -> u32 {
        self.ones
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParentRef {
    Base(u32),
    Delta(u32),
    None,
}

#[derive(Clone, Debug)]
pub struct DeltaRecord {
    pub name: String,
    pub parent: ParentRef,
    pub mtime_secs: u32,
    pub is_dir: bool,
    pub live: bool,
}

#[derive(Clone, Debug)]
pub struct Delta {
    pub tombstones: Bitset,
    pub added: Vec<DeltaRecord>,
    pub added_frns: HashMap<u64, u32>,
}

impl Delta {
    pub fn new(base_records: usize) -> Self {
        Self {
            tombstones: Bitset::new(base_records),
            added: Vec::new(),
            added_frns: HashMap::new(),
        }
    }

    pub fn live_added(&self) -> impl Iterator<Item = (u32, &DeltaRecord)> {
        self.added
            .iter()
            .enumerate()
            .filter(|(_, record)| record.live)
            .map(|(index, record)| (index as u32, record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitset_set_get_and_count() {
        let mut bits = Bitset::new(130);
        assert!(!bits.get(0));
        assert!(bits.set(0));
        assert!(!bits.set(0));
        assert!(bits.set(64));
        assert!(bits.set(129));
        assert!(bits.get(0));
        assert!(bits.get(64));
        assert!(bits.get(129));
        assert!(!bits.get(128));
        assert_eq!(bits.count_ones(), 3);
    }
}
