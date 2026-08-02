use crate::bitvec::{build_superblocks, RankSelect};
use crate::delta::{Delta, ParentRef};
use crate::{ArchivedArena, PARENT_NONE};

const NO_DIR: u32 = u32::MAX;

/// Derived directory topology in dense directory-ordinal space.
pub struct PathIndex {
    dir_bits: Vec<u8>,
    dir_superblocks: Vec<u32>,
    dir_parent: Vec<u32>,
    dir_order: Vec<u32>,
    records: usize,
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
        let depths: Vec<u16> = (0..directories as u32)
            .map(|directory| depth(directory, &dir_parent))
            .collect();
        let mut counts = [0usize; 513];
        for &value in &depths {
            counts[value as usize] += 1;
        }
        let mut offsets = [0usize; 513];
        let mut next = 0usize;
        for (offset, count) in offsets.iter_mut().zip(counts) {
            *offset = next;
            next += count;
        }
        let mut dir_order = vec![0u32; directories];
        for (directory, &value) in depths.iter().enumerate() {
            dir_order[offsets[value as usize]] = directory as u32;
            offsets[value as usize] += 1;
        }
        Self {
            dir_bits,
            dir_superblocks,
            dir_parent,
            dir_order,
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

    pub fn parent_dir_ord(&self, arena: &ArchivedArena, delta: &Delta, record: u32) -> Option<u32> {
        combined_parent(arena, delta, record).and_then(|parent| self.dir_ord(parent))
    }

    pub fn directory_count(&self) -> usize {
        self.dir_parent.len()
    }

    pub fn heap_bytes(&self) -> usize {
        self.dir_bits.len()
            + self.dir_superblocks.len() * std::mem::size_of::<u32>()
            + self.dir_parent.len() * std::mem::size_of::<u32>()
            + self.dir_order.len() * std::mem::size_of::<u32>()
    }

    pub fn closure(&self, mask: &mut [u16]) {
        assert_eq!(mask.len(), self.dir_parent.len());
        for &directory in &self.dir_order {
            let parent = self.dir_parent[directory as usize];
            if parent != NO_DIR && parent != directory {
                mask[directory as usize] |= mask[parent as usize];
            }
        }
    }

    pub fn closure_sparse(&self, mask: &mut [u16], touched: &mut Vec<u32>) {
        assert_eq!(mask.len(), self.dir_parent.len());
        for &directory in &self.dir_order {
            let index = directory as usize;
            let parent = self.dir_parent[index];
            if parent == NO_DIR || parent == directory {
                continue;
            }
            let old = mask[index];
            mask[index] |= mask[parent as usize];
            if old == 0 && mask[index] != 0 {
                touched.push(directory);
            }
        }
    }
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

fn depth(mut directory: u32, parents: &[u32]) -> u16 {
    let mut depth = 0u16;
    for _ in 0..512 {
        let parent = parents[directory as usize];
        if parent == NO_DIR || parent == directory {
            return depth;
        }
        directory = parent;
        depth = depth.saturating_add(1);
    }
    512
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Arena;

    fn archived(arena: &Arena) -> &'static ArchivedArena {
        let bytes = rkyv::to_bytes::<_, 1024>(arena).unwrap();
        crate::store::archived_bytes(Box::leak(bytes.into_boxed_slice())).unwrap()
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

        index.closure_sparse(&mut mask, &mut touched);
        assert_eq!(mask, vec![1, 1, 1]);
        assert_eq!(touched.len(), 3);

        for directory in touched.drain(..) {
            mask[directory as usize] = 0;
        }
        assert!(mask.iter().all(|&value| value == 0));
    }
}
