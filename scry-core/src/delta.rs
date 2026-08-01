use std::collections::HashMap;

use crate::store::ArenaStore;

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
    pub size_bytes: u64,
    pub live: bool,
}

#[derive(Clone, Debug)]
pub struct Delta {
    pub tombstones: Bitset,
    pub added: Vec<DeltaRecord>,
    pub added_frns: HashMap<u64, u32>,
}

#[derive(Clone, Debug)]
pub enum DeltaEvent {
    Created {
        frn: u64,
        parent_frn: u64,
        name: String,
        is_dir: bool,
        mtime_secs: u32,
    },
    Deleted {
        frn: u64,
    },
    Renamed {
        frn: u64,
        parent_frn: u64,
        name: String,
    },
    Modified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    NeedsFullReindex,
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

    pub fn apply(&mut self, event: &DeltaEvent, base: &ArenaStore) -> ApplyOutcome {
        if base.frn_map.is_none() {
            return match event {
                DeltaEvent::Modified => ApplyOutcome::Applied,
                _ => ApplyOutcome::NeedsFullReindex,
            };
        }
        match event {
            DeltaEvent::Created {
                frn,
                parent_frn,
                name,
                is_dir,
                mtime_secs,
            } => self.create(
                *frn,
                *parent_frn,
                name.clone(),
                (*is_dir, *mtime_secs, 0),
                base,
            ),
            DeltaEvent::Deleted { frn } => {
                self.delete(*frn, base);
                ApplyOutcome::Applied
            }
            DeltaEvent::Renamed {
                frn,
                parent_frn,
                name,
            } => {
                let Some((is_dir, mtime_secs, size_bytes)) = self.metadata(*frn, base) else {
                    return ApplyOutcome::NeedsFullReindex;
                };
                self.delete(*frn, base);
                self.create(
                    *frn,
                    *parent_frn,
                    name.clone(),
                    (is_dir, mtime_secs, size_bytes),
                    base,
                )
            }
            DeltaEvent::Modified => ApplyOutcome::Applied,
        }
    }

    fn resolve_parent(&self, frn: u64, base: &ArenaStore) -> Option<ParentRef> {
        if let Some(&index) = self.added_frns.get(&frn) {
            return self.added[index as usize]
                .live
                .then_some(ParentRef::Delta(index));
        }
        base.frn_map
            .as_ref()
            .and_then(|map| map.lookup(frn))
            .map(ParentRef::Base)
    }

    fn create(
        &mut self,
        frn: u64,
        parent_frn: u64,
        name: String,
        metadata: (bool, u32, u64),
        base: &ArenaStore,
    ) -> ApplyOutcome {
        let Some(parent) = self.resolve_parent(parent_frn, base) else {
            return ApplyOutcome::NeedsFullReindex;
        };
        let index = self.added.len() as u32;
        let (is_dir, mtime_secs, size_bytes) = metadata;
        self.added.push(DeltaRecord {
            name,
            parent,
            mtime_secs,
            is_dir,
            size_bytes,
            live: true,
        });
        self.added_frns.insert(frn, index);
        ApplyOutcome::Applied
    }

    fn delete(&mut self, frn: u64, base: &ArenaStore) {
        if let Some(index) = self.added_frns.remove(&frn) {
            self.added[index as usize].live = false;
        } else if let Some(index) = base.frn_map.as_ref().and_then(|map| map.lookup(frn)) {
            self.tombstones.set(index);
        }
    }

    fn metadata(&self, frn: u64, base: &ArenaStore) -> Option<(bool, u32, u64)> {
        if let Some(&index) = self.added_frns.get(&frn) {
            let record = &self.added[index as usize];
            return record
                .live
                .then_some((record.is_dir, record.mtime_secs, record.size_bytes));
        }
        let index = base.frn_map.as_ref()?.lookup(frn)?;
        let arena = base.archived();
        Some((arena.is_dir(index), arena.mtime(index), arena.size_bytes(index)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_store() -> (tempfile::TempDir, ArenaStore) {
        let mut builder = crate::ArenaBuilder::default();
        let root = builder.push_bytes_with_frn(b"C:", 0, true, 5);
        let file = builder.push_bytes_with_frn(b"old.txt", 7, false, 10);
        builder.set_parent(file, root);
        let (arena, mut frns) = builder.build();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.rkyv");
        crate::store::save_with_sidecar(&arena, &mut frns, &path, |_| {}, |_| {}).unwrap();
        let store = ArenaStore::open(&path).unwrap();
        (dir, store)
    }

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

    #[test]
    fn delta_applies_create_delete_and_rename() {
        let (_dir, base) = base_store();
        let mut delta = Delta::new(base.archived().len());
        assert_eq!(
            delta.apply(
                &DeltaEvent::Created {
                    frn: 20,
                    parent_frn: 5,
                    name: "new.txt".into(),
                    is_dir: false,
                    mtime_secs: 1,
                },
                &base,
            ),
            ApplyOutcome::Applied
        );
        assert_eq!(delta.added_frns.get(&20), Some(&0));

        assert_eq!(
            delta.apply(
                &DeltaEvent::Renamed {
                    frn: 20,
                    parent_frn: 5,
                    name: "renamed.txt".into(),
                },
                &base,
            ),
            ApplyOutcome::Applied
        );
        assert!(!delta.added[0].live);
        assert_eq!(delta.added[1].name, "renamed.txt");

        delta.apply(&DeltaEvent::Deleted { frn: 10 }, &base);
        let old_index = base.frn_map.as_ref().unwrap().lookup(10).unwrap();
        assert!(delta.tombstones.get(old_index));
    }

    #[test]
    fn delta_preserves_delta_parent_identity() {
        let (_dir, base) = base_store();
        let mut delta = Delta::new(base.archived().len());
        delta.apply(
            &DeltaEvent::Created {
                frn: 20,
                parent_frn: 5,
                name: "X".into(),
                is_dir: true,
                mtime_secs: 0,
            },
            &base,
        );
        delta.apply(
            &DeltaEvent::Created {
                frn: 21,
                parent_frn: 20,
                name: "y".into(),
                is_dir: false,
                mtime_secs: 0,
            },
            &base,
        );
        assert_eq!(delta.added[1].parent, ParentRef::Delta(0));
    }

    #[test]
    fn delta_unknown_parent_requests_full_reindex_and_modified_is_noop() {
        let (_dir, base) = base_store();
        let mut delta = Delta::new(base.archived().len());
        let before = delta.clone();
        assert_eq!(
            delta.apply(&DeltaEvent::Modified, &base),
            ApplyOutcome::Applied
        );
        assert_eq!(delta.added.len(), before.added.len());
        assert_eq!(
            delta.apply(
                &DeltaEvent::Created {
                    frn: 99,
                    parent_frn: 404,
                    name: "orphan".into(),
                    is_dir: false,
                    mtime_secs: 0,
                },
                &base,
            ),
            ApplyOutcome::NeedsFullReindex
        );
    }
}
