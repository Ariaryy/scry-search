use crate::record::{ArchivedFileRecord, FileRecord};
use rkyv::{Archive, Deserialize, Serialize};

/// The full index: a flat struct-of-records arena plus a name-sorted
/// permutation for O(log n) prefix search. This whole thing is what gets
/// rkyv-serialized to disk and mmap'd back zero-copy on daemon start —
/// there is no parse step between "bytes on disk" and "queryable index".
#[derive(Archive, Serialize, Deserialize, Debug)]
#[archive(check_bytes)]
pub struct Arena {
    pub records: Vec<FileRecord>,
    /// Indices into `records`, sorted by `records[i].name` case-insensitively.
    /// Binary-search this for prefix queries instead of scanning `records` linearly.
    pub name_order: Vec<u32>,
}

impl Arena {
    pub fn builder() -> ArenaBuilder {
        ArenaBuilder::default()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl ArchivedArena {
    /// Reconstruct a full path by walking `parent` links. Only done lazily,
    /// on the results that actually get returned to a client — never during
    /// indexing or search.
    pub fn full_path(&self, mut idx: u32, sep: char) -> String {
        let mut parts: Vec<&str> = Vec::new();
        loop {
            let rec: &ArchivedFileRecord = &self.records[idx as usize];
            parts.push(rec.name.as_str());
            if rec.parent == u32::MAX {
                break;
            }
            idx = rec.parent;
        }
        parts.reverse();
        parts.join(&sep.to_string())
    }

    /// Binary search the name_order permutation for the first/last index whose
    /// record name starts with `prefix` (ASCII case-insensitive).
    pub fn prefix_range(&self, prefix: &str) -> std::ops::Range<usize> {
        let prefix_lower = prefix.to_ascii_lowercase();
        let lo = self.name_order.partition_point(|&i| {
            name_lower(&self.records[i as usize]).as_str() < prefix_lower.as_str()
        });
        let hi = self.name_order.partition_point(|&i| {
            let n = name_lower(&self.records[i as usize]);
            n.starts_with(prefix_lower.as_str()) || n.as_str() < prefix_lower.as_str()
        });
        lo..hi
    }

    pub fn name_at_sorted(&self, pos: usize) -> u32 {
        self.name_order[pos]
    }
}

fn name_lower(rec: &ArchivedFileRecord) -> String {
    rec.name.to_ascii_lowercase()
}

#[derive(Default)]
pub struct ArenaBuilder {
    records: Vec<FileRecord>,
}

impl ArenaBuilder {
    /// Push a record, returns its arena index (use this as the `parent` for children).
    pub fn push(&mut self, record: FileRecord) -> u32 {
        let idx = self.records.len() as u32;
        self.records.push(record);
        idx
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Finalize: build the sorted name-order permutation and produce the Arena.
    /// This is the only O(n log n) step; everything else in ingest is O(1) amortized.
    pub fn build(mut self) -> Arena {
        let mut name_order: Vec<u32> = (0..self.records.len() as u32).collect();
        name_order.sort_unstable_by(|&a, &b| {
            let na = self.records[a as usize].name.to_ascii_lowercase();
            let nb = self.records[b as usize].name.to_ascii_lowercase();
            na.cmp(&nb).then(a.cmp(&b))
        });
        Arena {
            records: std::mem::take(&mut self.records),
            name_order,
        }
    }
}
