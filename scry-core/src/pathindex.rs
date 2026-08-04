use crate::bitvec::{build_superblocks, RankSelect};
use crate::delta::{Delta, ParentRef};
use crate::{ArchivedArena, PARENT_NONE};

const NO_DIR: u32 = u32::MAX;
const PACKED_PARENT_BITS: usize = 20;
const PACKED_NO_DIR: u32 = (1 << PACKED_PARENT_BITS) - 1;

enum DirParents {
    Packed20 { words: Vec<u64>, len: usize },
    Wide(Vec<u32>),
}

impl DirParents {
    fn new(parents: Vec<u32>) -> Self {
        if parents.len() >= PACKED_NO_DIR as usize {
            return Self::Wide(parents);
        }
        let len = parents.len();
        let mut words = vec![0u64; (len * PACKED_PARENT_BITS).div_ceil(64)];
        for (index, parent) in parents.into_iter().enumerate() {
            let value = if parent == NO_DIR {
                PACKED_NO_DIR
            } else {
                parent
            } as u64;
            let bit = index * PACKED_PARENT_BITS;
            let word = bit / 64;
            let shift = bit % 64;
            words[word] |= value << shift;
            if shift + PACKED_PARENT_BITS > 64 {
                words[word + 1] |= value >> (64 - shift);
            }
        }
        Self::Packed20 { words, len }
    }

    fn len(&self) -> usize {
        match self {
            Self::Packed20 { len, .. } => *len,
            Self::Wide(parents) => parents.len(),
        }
    }

    fn get(&self, index: usize) -> u32 {
        match self {
            Self::Packed20 { words, len } => {
                debug_assert!(index < *len);
                let bit = index * PACKED_PARENT_BITS;
                let word = bit / 64;
                let shift = bit % 64;
                let mut value = words[word] >> shift;
                if shift + PACKED_PARENT_BITS > 64 {
                    value |= words[word + 1] << (64 - shift);
                }
                let value = value as u32 & PACKED_NO_DIR;
                if value == PACKED_NO_DIR {
                    NO_DIR
                } else {
                    value
                }
            }
            Self::Wide(parents) => parents[index],
        }
    }

    fn heap_bytes(&self) -> usize {
        match self {
            Self::Packed20 { words, .. } => words.len() * std::mem::size_of::<u64>(),
            Self::Wide(parents) => parents.len() * std::mem::size_of::<u32>(),
        }
    }
}

/// Derived directory topology in dense directory-ordinal space.
pub struct PathIndex {
    dir_bits: Vec<u8>,
    dir_superblocks: Vec<u32>,
    dir_parent: DirParents,
    records: usize,
}

#[derive(Default)]
pub struct PathClosureScratch {
    resolved: Vec<u8>,
    stack: Vec<u32>,
}

impl PathClosureScratch {
    fn reset(&mut self, directories: usize) {
        self.resolved.resize(directories.div_ceil(8), 0);
        self.resolved.fill(0);
        self.stack.clear();
    }
}

impl PathIndex {
    pub fn build(arena: &ArchivedArena, delta: &Delta) -> Self {
        let records = arena.len() + delta.added.len();
        let mut dir_bits = vec![0u8; records.div_ceil(8)];
        for record in 0..arena.len() as u32 {
            if arena.is_dir(record) && !delta.tombstones.get(record) {
                dir_bits[record as usize / 8] |= 1 << (record % 8);
            }
        }
        for (index, record) in delta.added.iter().enumerate() {
            if record.live && record.is_dir {
                let combined = arena.len() + index;
                dir_bits[combined / 8] |= 1 << (combined % 8);
            }
        }
        let dir_superblocks = build_superblocks(&dir_bits);
        let rank = RankSelect::new(&dir_bits, &dir_superblocks);
        let directories = rank.rank1(records);
        let mut dir_parent = vec![NO_DIR; directories];
        for record in 0..records as u32 {
            if !bit(&dir_bits, record as usize) {
                continue;
            }
            let ordinal = rank.rank1(record as usize);
            if let Some(parent) = combined_parent(arena, delta, record) {
                if bit(&dir_bits, parent as usize) {
                    dir_parent[ordinal] = rank.rank1(parent as usize) as u32;
                }
            }
        }
        Self {
            dir_bits,
            dir_superblocks,
            dir_parent: DirParents::new(dir_parent),
            records,
        }
    }

    pub fn records(&self) -> usize {
        self.records
    }

    pub fn dir_ord(&self, record: u32) -> Option<u32> {
        bit(&self.dir_bits, record as usize).then(|| {
            RankSelect::new(&self.dir_bits, &self.dir_superblocks).rank1(record as usize) as u32
        })
    }

    pub fn dir_record(&self, directory: u32) -> Option<u32> {
        RankSelect::new(&self.dir_bits, &self.dir_superblocks)
            .select1(directory as usize)
            .map(|record| record as u32)
    }

    pub fn parent_dir_ord(&self, arena: &ArchivedArena, delta: &Delta, record: u32) -> Option<u32> {
        combined_parent(arena, delta, record).and_then(|parent| self.dir_ord(parent))
    }

    pub fn parent_record(&self, arena: &ArchivedArena, delta: &Delta, record: u32) -> Option<u32> {
        combined_parent(arena, delta, record)
    }

    pub fn directory_count(&self) -> usize {
        self.dir_parent.len()
    }

    pub fn heap_bytes(&self) -> usize {
        self.dir_bits.len()
            + self.dir_superblocks.len() * std::mem::size_of::<u32>()
            + self.dir_parent.heap_bytes()
    }

    pub fn closure(&self, mask: &mut [u16]) {
        self.closure_sparse(mask, &mut Vec::new(), &mut PathClosureScratch::default());
    }

    pub fn closure_sparse(
        &self,
        mask: &mut [u16],
        touched: &mut Vec<u32>,
        scratch: &mut PathClosureScratch,
    ) {
        assert_eq!(mask.len(), self.dir_parent.len());
        scratch.reset(self.dir_parent.len());
        for start in 0..self.dir_parent.len() as u32 {
            if bit(&scratch.resolved, start as usize) {
                continue;
            }
            scratch.stack.clear();
            let mut current = start;
            for _ in 0..512 {
                if bit(&scratch.resolved, current as usize) {
                    break;
                }
                if let Some(cycle_start) = scratch.stack.iter().position(|&dir| dir == current) {
                    let cycle_mask = scratch.stack[cycle_start..]
                        .iter()
                        .fold(0u16, |combined, &dir| combined | mask[dir as usize]);
                    for &directory in &scratch.stack[cycle_start..] {
                        let old = mask[directory as usize];
                        mask[directory as usize] = cycle_mask;
                        set_bit(&mut scratch.resolved, directory as usize);
                        if old == 0 && cycle_mask != 0 {
                            touched.push(directory);
                        }
                    }
                    scratch.stack.truncate(cycle_start);
                    break;
                }
                scratch.stack.push(current);
                let parent = self.dir_parent.get(current as usize);
                if parent == NO_DIR || parent == current {
                    break;
                }
                current = parent;
            }

            while let Some(directory) = scratch.stack.pop() {
                let old = mask[directory as usize];
                let parent = self.dir_parent.get(directory as usize);
                if parent != NO_DIR
                    && parent != directory
                    && bit(&scratch.resolved, parent as usize)
                {
                    mask[directory as usize] |= mask[parent as usize];
                }
                set_bit(&mut scratch.resolved, directory as usize);
                if old == 0 && mask[directory as usize] != 0 {
                    touched.push(directory);
                }
            }
        }
    }
}

fn set_bit(bits: &mut [u8], position: usize) {
    bits[position / 8] |= 1 << (position % 8);
}

fn bit(bits: &[u8], position: usize) -> bool {
    bits.get(position / 8)
        .is_some_and(|byte| byte & (1 << (position % 8)) != 0)
}

fn combined_parent(arena: &ArchivedArena, delta: &Delta, record: u32) -> Option<u32> {
    if record < arena.len() as u32 {
        let parent = arena.parent(record);
        return (parent != PARENT_NONE).then_some(parent);
    }
    let record = delta.added.get(record as usize - arena.len())?;
    match record.parent {
        ParentRef::Base(parent) => Some(parent),
        ParentRef::Delta(parent) => Some(arena.len() as u32 + parent),
        ParentRef::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Arena;

    fn archived(arena: &Arena) -> &'static ArchivedArena {
        // Leak the `AlignedVec` itself rather than a re-boxed slice: the
        // archive's alignment comes from that allocation.
        let bytes: &'static _ = Box::leak(Box::new(crate::store::to_bytes(arena).unwrap()));
        crate::store::archived_bytes(bytes).unwrap()
    }

    #[test]
    fn pathindex_dir_ord_matches_a_brute_force_rank() {
        let mut builder = Arena::builder();
        builder.push("root", 0, true);
        builder.push("file", 0, false);
        builder.push("dir", 0, true);
        let arena = archived(&builder.build().0);
        let delta = Delta::new(arena.len());
        let index = PathIndex::build(arena, &delta);
        assert_eq!(index.dir_ord(0), Some(0));
        assert_eq!(index.dir_ord(1), None);
        assert_eq!(index.dir_ord(2), Some(1));
        assert_eq!(index.dir_record(0), Some(0));
        assert_eq!(index.dir_record(1), Some(2));
    }

    #[test]
    fn packed_directory_parents_round_trip_word_boundaries() {
        let expected: Vec<u32> = (0..257)
            .map(|index| if index % 17 == 0 { NO_DIR } else { index - 1 })
            .collect();
        let packed = DirParents::new(expected.clone());
        assert_eq!(packed.len(), expected.len());
        for (index, value) in expected.into_iter().enumerate() {
            assert_eq!(packed.get(index), value);
        }
    }

    #[test]
    fn pathindex_closure_terminates_on_a_cycle() {
        let mut builder = Arena::builder();
        let a = builder.push("a", 0, true);
        let b = builder.push("b", 0, true);
        builder.set_parent(a, b);
        builder.set_parent(b, a);
        let arena = archived(&builder.build().0);
        let delta = Delta::new(arena.len());
        let index = PathIndex::build(arena, &delta);
        let mut mask = vec![0u16; index.directory_count()];
        mask[0] = 1;
        index.closure(&mut mask);
        assert_eq!(mask.len(), 2);
    }

    #[test]
    fn sparse_closure_tracks_every_nonzero_directory_for_clearing() {
        let mut builder = Arena::builder();
        let root = builder.push("root", 0, true);
        let child = builder.push("child", 0, true);
        let grandchild = builder.push("grandchild", 0, true);
        builder.set_parent(child, root);
        builder.set_parent(grandchild, child);
        let arena = archived(&builder.build().0);
        let delta = Delta::new(arena.len());
        let index = PathIndex::build(arena, &delta);
        let root = index.dir_ord(arena.prefix_range("root").start).unwrap();
        let mut mask = vec![0u16; index.directory_count()];
        let mut touched = vec![root];
        mask[root as usize] = 1;

        index.closure_sparse(&mut mask, &mut touched, &mut PathClosureScratch::default());
        assert_eq!(mask, vec![1, 1, 1]);
        assert_eq!(touched.len(), 3);

        for directory in touched.drain(..) {
            mask[directory as usize] = 0;
        }
        assert!(mask.iter().all(|&value| value == 0));
    }

    #[test]
    fn compressed_closure_matches_a_direct_parent_walk() {
        let mut builder = Arena::builder();
        let mut records = Vec::new();
        for index in 0..1_000u32 {
            records.push(builder.push(
                &format!("dir_{:04}", index.wrapping_mul(613) % 1_009),
                0,
                true,
            ));
        }
        for index in 1..records.len() {
            builder.set_parent(records[index], records[(index - 1) / 2]);
        }
        let arena = archived(&builder.build().0);
        let delta = Delta::new(arena.len());
        let index = PathIndex::build(arena, &delta);
        let direct: Vec<u16> = (0..index.directory_count())
            .map(|directory| match directory {
                value if value % 97 == 0 => 1,
                value if value % 131 == 0 => 2,
                _ => 0,
            })
            .collect();
        let mut expected = direct.clone();
        for (directory, expected_mask) in expected.iter_mut().enumerate() {
            let mut current = directory as u32;
            for _ in 0..512 {
                let parent = index.dir_parent.get(current as usize);
                if parent == NO_DIR || parent == current {
                    break;
                }
                *expected_mask |= direct[parent as usize];
                current = parent;
            }
        }

        let mut actual = direct;
        index.closure(&mut actual);
        assert_eq!(actual, expected);
    }
}
