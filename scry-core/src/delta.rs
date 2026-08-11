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

    fn words(&self) -> &[u64] {
        &self.words
    }

    /// LSB-first byte view of the underlying words, for building a
    /// `crate::bitvec::RankSelect` index over this bitset without copying it.
    /// Compaction uses this to turn a per-record "how many tombstones come
    /// before me" query into an O(1) rank lookup instead of a dense
    /// `old index -> new index` translation array.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: u64 has no padding and accepts every bit pattern, so a
        // byte-wise reinterpretation of `words` is always valid; the
        // returned slice borrows `self` and cannot outlive it.
        unsafe {
            std::slice::from_raw_parts(
                self.words.as_ptr().cast::<u8>(),
                std::mem::size_of_val(self.words.as_slice()),
            )
        }
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
    pub size_exact: bool,
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
                (*is_dir, *mtime_secs, 0, false),
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
                if let (Some(&index), Some(parent)) = (
                    self.added_frns.get(frn),
                    self.resolve_parent(*parent_frn, base),
                ) {
                    let record = &self.added[index as usize];
                    if record.live && record.parent == parent && record.name == *name {
                        return ApplyOutcome::Applied;
                    }
                }
                let Some((is_dir, mtime_secs, size_bytes, size_exact)) = self.metadata(*frn, base)
                else {
                    return ApplyOutcome::NeedsFullReindex;
                };
                self.delete(*frn, base);
                self.create(
                    *frn,
                    *parent_frn,
                    name.clone(),
                    (is_dir, mtime_secs, size_bytes, size_exact),
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
        metadata: (bool, u32, u64, bool),
        base: &ArenaStore,
    ) -> ApplyOutcome {
        if self
            .added_frns
            .get(&frn)
            .is_some_and(|&index| self.added[index as usize].live)
        {
            return ApplyOutcome::Applied;
        }
        if let Some(index) = base.frn_map.as_ref().and_then(|map| map.lookup(frn)) {
            if !self.tombstones.get(index) {
                return ApplyOutcome::Applied;
            }
        }
        let Some(parent) = self.resolve_parent(parent_frn, base) else {
            return ApplyOutcome::NeedsFullReindex;
        };
        let index = self.added.len() as u32;
        let (is_dir, mtime_secs, size_bytes, size_exact) = metadata;
        self.added.push(DeltaRecord {
            name,
            parent,
            mtime_secs,
            is_dir,
            size_bytes,
            size_exact,
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

    fn metadata(&self, frn: u64, base: &ArenaStore) -> Option<(bool, u32, u64, bool)> {
        if let Some(&index) = self.added_frns.get(&frn) {
            let record = &self.added[index as usize];
            return record.live.then_some((
                record.is_dir,
                record.mtime_secs,
                record.size_bytes,
                record.size_exact,
            ));
        }
        let index = base.frn_map.as_ref()?.lookup(frn)?;
        let arena = base.archived();
        Some((
            arena.is_dir(index),
            arena.mtime(index),
            arena.size_bytes(index),
            arena.size_exact(index),
        ))
    }
}

impl Delta {
    pub fn encode_query_overlay(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.tombstones.words().len() as u32).to_le_bytes());
        for &word in self.tombstones.words() {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&(self.added.len() as u32).to_le_bytes());
        for record in &self.added {
            out.push(record.live as u8);
            match record.parent {
                ParentRef::None => out.push(0),
                ParentRef::Base(index) => {
                    out.push(1);
                    out.extend_from_slice(&index.to_le_bytes());
                }
                ParentRef::Delta(index) => {
                    out.push(2);
                    out.extend_from_slice(&index.to_le_bytes());
                }
            }
            out.extend_from_slice(&record.mtime_secs.to_le_bytes());
            out.push(record.is_dir as u8);
            out.extend_from_slice(&record.size_bytes.to_le_bytes());
            out.extend_from_slice(&(record.name.len() as u32).to_le_bytes());
            out.extend_from_slice(record.name.as_bytes());
        }
        out.extend_from_slice(b"SCSE");
        out.push(1);
        let mut flags = vec![0u8; self.added.len().div_ceil(8)];
        for (index, record) in self.added.iter().enumerate() {
            if record.size_exact {
                flags[index / 8] |= 1 << (index % 8);
            }
        }
        out.extend_from_slice(&flags);
        out
    }

    pub fn decode_query_overlay(bytes: &[u8], base_records: usize) -> Option<Self> {
        fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
            let value = bytes.get(..len)?;
            *bytes = bytes.get(len..)?;
            Some(value)
        }
        fn u32_value(bytes: &mut &[u8]) -> Option<u32> {
            Some(u32::from_le_bytes(take(bytes, 4)?.try_into().ok()?))
        }
        fn u64_value(bytes: &mut &[u8]) -> Option<u64> {
            Some(u64::from_le_bytes(take(bytes, 8)?.try_into().ok()?))
        }

        let mut input = bytes;
        let word_count = u32_value(&mut input)? as usize;
        if word_count != base_records.div_ceil(64) {
            return None;
        }
        let mut tombstones = Bitset::new(base_records);
        for word_index in 0..word_count {
            let word = u64_value(&mut input)?;
            for bit in 0..64 {
                if word & (1 << bit) != 0 {
                    let index = word_index * 64 + bit;
                    if index < base_records {
                        tombstones.set(index as u32);
                    }
                }
            }
        }
        let count = u32_value(&mut input)? as usize;
        if count > base_records / 10 + 1_000_000 {
            return None;
        }
        let mut added = Vec::with_capacity(count);
        for index in 0..count {
            let live = *take(&mut input, 1)?.first()? != 0;
            let parent = match *take(&mut input, 1)?.first()? {
                0 => ParentRef::None,
                1 => ParentRef::Base(u32_value(&mut input)?),
                2 => {
                    let parent = u32_value(&mut input)?;
                    if parent as usize >= index {
                        return None;
                    }
                    ParentRef::Delta(parent)
                }
                _ => return None,
            };
            let mtime_secs = u32_value(&mut input)?;
            let is_dir = *take(&mut input, 1)?.first()? != 0;
            let size_bytes = u64_value(&mut input)?;
            let name_len = u32_value(&mut input)? as usize;
            let name = std::str::from_utf8(take(&mut input, name_len)?)
                .ok()?
                .to_owned();
            added.push(DeltaRecord {
                name,
                parent,
                mtime_secs,
                is_dir,
                size_bytes,
                size_exact: !is_dir && size_bytes != 0,
                live,
            });
        }
        if input.starts_with(b"SCSE") {
            input = input.get(4..)?;
            if *take(&mut input, 1)?.first()? != 1 {
                return None;
            }
            let flags = take(&mut input, count.div_ceil(8))?;
            for (index, record) in added.iter_mut().enumerate() {
                record.size_exact = flags[index / 8] & (1 << (index % 8)) != 0;
            }
            if !input.is_empty() {
                return None;
            }
        }
        Some(Self {
            tombstones,
            added,
            added_frns: HashMap::new(),
        })
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
    fn replaying_events_is_idempotent() {
        let (_dir, base) = base_store();
        let events = [
            DeltaEvent::Created {
                frn: 20,
                parent_frn: 5,
                name: "new.txt".into(),
                is_dir: false,
                mtime_secs: 1,
            },
            DeltaEvent::Renamed {
                frn: 20,
                parent_frn: 5,
                name: "renamed.txt".into(),
            },
            DeltaEvent::Deleted { frn: 10 },
        ];
        let mut delta = Delta::new(base.archived().len());
        for event in &events {
            assert_eq!(delta.apply(event, &base), ApplyOutcome::Applied);
        }
        let once = delta.encode_query_overlay();
        for event in &events {
            assert_eq!(delta.apply(event, &base), ApplyOutcome::Applied);
        }
        assert_eq!(delta.encode_query_overlay(), once);
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

    #[test]
    fn query_overlay_round_trips_and_rejects_truncation() {
        let (_dir, base) = base_store();
        let mut delta = Delta::new(base.archived().len());
        assert_eq!(
            delta.apply(
                &DeltaEvent::Created {
                    frn: 20,
                    parent_frn: 5,
                    name: "new.txt".into(),
                    is_dir: false,
                    mtime_secs: 7,
                },
                &base,
            ),
            ApplyOutcome::Applied
        );
        let encoded = delta.encode_query_overlay();
        let decoded = Delta::decode_query_overlay(&encoded, base.archived().len()).unwrap();
        assert_eq!(decoded.added[0].name, "new.txt");
        assert_eq!(decoded.added[0].mtime_secs, 7);
        assert!(!decoded.added[0].size_exact);
        let legacy = &encoded[..encoded.len() - 6];
        assert!(
            !Delta::decode_query_overlay(legacy, base.archived().len())
                .unwrap()
                .added[0]
                .size_exact
        );
        assert!(
            Delta::decode_query_overlay(&encoded[..encoded.len() - 1], base.archived().len())
                .is_none()
        );
    }
}
