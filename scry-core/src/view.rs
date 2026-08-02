use std::sync::Arc;

use regex_automata::meta::Regex;
use regex_automata::util::syntax;

use crate::ascii;
use crate::delta::{Delta, ParentRef};
use crate::protocol::ResultEntry;
use crate::query::{search_base, Query};
use crate::store::ArenaStore;
use crate::{Arena, ArenaBuilder, FrnEntry, PARENT_NONE};

/// Immutable base-and-overlay pair published through one atomic pointer.
pub struct IndexView {
    pub base: Arc<ArenaStore>,
    pub delta: Arc<Delta>,
    pub generation: u64,
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::delta::{DeltaRecord, ParentRef};

    fn base_view(count: usize) -> (tempfile::TempDir, IndexView) {
        let mut builder = crate::ArenaBuilder::default();
        let root = builder.push("C:", 0, true);
        for i in 0..count {
            let child = builder.push(&format!("match_{i:04}.txt"), 0, false);
            builder.set_parent(child, root);
        }
        let (arena, _) = builder.build();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("view.rkyv");
        crate::store::save(&arena, &path).unwrap();
        let base = Arc::new(ArenaStore::open(&path).unwrap());
        (dir, IndexView::new(base))
    }

    #[test]
    fn empty_delta_search_matches_base_search() {
        let (_dir, view) = base_view(5_000);
        for i in 0..100 {
            let query = Query::Substring(format!("{:02}", i));
            let expected: Vec<String> = search_base(view.base.archived(), &query, usize::MAX)
                .into_iter()
                .map(|index| view.base.archived().full_path(index, '\\'))
                .collect();
            let actual: Vec<String> = view
                .search(&query, usize::MAX)
                .into_iter()
                .map(|entry| entry.path)
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn limit_is_applied_across_both_layers() {
        let (_dir, mut view) = base_view(50);
        let root = view.base.archived().prefix_range("C:").start;
        let mut delta = Delta::new(view.base.archived().len());
        for i in 0..5 {
            delta.added.push(DeltaRecord {
                name: format!("match_delta_{i}.txt"),
                parent: ParentRef::Base(root),
                mtime_secs: 0,
                is_dir: false,
                size_bytes: 0,
                live: true,
            });
        }
        view.delta = Arc::new(delta);
        assert_eq!(view.search(&Query::Prefix("match".into()), 10).len(), 10);
        let all = view.search(&Query::Prefix("match".into()), 100);
        assert_eq!(all.len(), 55);
        assert_eq!(
            all.iter()
                .filter(|entry| entry.path.contains("delta"))
                .count(),
            5
        );
        assert_eq!(view.search(&Query::Substring("DELTA".into()), 100).len(), 5);
    }

    #[test]
    fn delta_child_path_uses_delta_parent() {
        let (_dir, mut view) = base_view(0);
        let root = view.base.archived().prefix_range("C:").start;
        let mut delta = Delta::new(view.base.archived().len());
        delta.added.push(DeltaRecord {
            name: "X".into(),
            parent: ParentRef::Base(root),
            mtime_secs: 0,
            is_dir: true,
            size_bytes: 0,
            live: true,
        });
        delta.added.push(DeltaRecord {
            name: "y".into(),
            parent: ParentRef::Delta(0),
            mtime_secs: 0,
            is_dir: false,
            size_bytes: 0,
            live: true,
        });
        view.delta = Arc::new(delta);
        assert!(view.delta_path(1).ends_with("X\\y"));
    }

    fn open_compacted(view: &IndexView) -> (tempfile::TempDir, IndexView) {
        let (arena, mut frns) = view.compact();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compacted.rkyv");
        crate::store::save_with_sidecar(&arena, &mut frns, &path, |_| {}, |_| {}).unwrap();
        let base = Arc::new(ArenaStore::open(&path).unwrap());
        (dir, IndexView::new(base))
    }

    #[test]
    fn compaction_preserves_query_results() {
        let (_dir, mut view) = base_view(200);
        let root = view.base.archived().prefix_range("C:").start;
        let deleted = view.base.archived().prefix_range("match_0042").start;
        let mut delta = Delta::new(view.base.archived().len());
        delta.tombstones.set(deleted);
        delta.added.push(DeltaRecord {
            name: "match_delta.txt".into(),
            parent: ParentRef::Base(root),
            mtime_secs: 0,
            is_dir: false,
            size_bytes: 0,
            live: true,
        });
        view.delta = Arc::new(delta);

        let before: Vec<Vec<String>> = (0..50)
            .map(|i| {
                let mut paths: Vec<_> = view
                    .search(&Query::Substring(format!("{:02}", i)), usize::MAX)
                    .into_iter()
                    .map(|entry| entry.path)
                    .collect();
                paths.sort();
                paths
            })
            .collect();
        let (_compacted_dir, compacted) = open_compacted(&view);
        for (i, expected) in before.into_iter().enumerate() {
            let mut actual: Vec<_> = compacted
                .search(&Query::Substring(format!("{:02}", i)), usize::MAX)
                .into_iter()
                .map(|entry| entry.path)
                .collect();
            actual.sort();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn compaction_removes_tombstones_and_empty_is_identity() {
        let (_dir, mut view) = base_view(20);
        let original = view.len();
        let empty = view.compact().0;
        assert_eq!(empty.len(), original);

        let mut delta = Delta::new(view.base.archived().len());
        assert!(delta.tombstones.set(3));
        assert!(delta.tombstones.set(7));
        view.delta = Arc::new(delta);
        let compacted = view.compact().0;
        assert_eq!(compacted.len(), original - 2);
    }

    #[test]
    fn shared_overlay_search_matches_rpc_search() {
        let (_dir, mut view) = base_view(200);
        let root = view.base.archived().prefix_range("C:").start;
        let deleted = view.base.archived().prefix_range("match_0042").start;
        let mut delta = Delta::new(view.base.archived().len());
        assert!(delta.tombstones.set(deleted));
        delta.added.push(DeltaRecord {
            name: "match_delta.txt".into(),
            parent: ParentRef::Base(root),
            mtime_secs: 0,
            is_dir: false,
            size_bytes: 17,
            live: true,
        });
        view.delta = Arc::new(delta);
        let encoded = view.delta.encode_query_overlay();
        let decoded = Delta::decode_query_overlay(&encoded, view.base.archived().len()).unwrap();
        for query in [
            Query::Prefix("match".into()),
            Query::Substring("DELTA".into()),
            Query::wildcard("*.txt"),
        ] {
            assert_eq!(
                search_archived_with_delta(view.base.archived(), &decoded, &query, 100),
                view.search(&query, 100)
            );
        }
    }
}

impl IndexView {
    pub fn new(base: Arc<ArenaStore>) -> Self {
        let records = base.archived().len();
        Self {
            base,
            delta: Arc::new(Delta::new(records)),
            generation: fresh_generation(),
        }
    }

    pub fn len(&self) -> usize {
        self.base.archived().len() - self.delta.tombstones.count_ones() as usize
            + self.delta.live_added().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Search the sorted base first, followed by live overlay records in
    /// creation order. The combined result is intentionally not globally
    /// sorted while the overlay remains uncompacted.
    pub fn search(&self, query: &Query, limit: usize) -> Vec<ResultEntry> {
        let arena = self.base.archived();
        let mut entries = Vec::new();
        for index in search_base(arena, query, usize::MAX) {
            if self.delta.tombstones.get(index) {
                continue;
            }
            entries.push(ResultEntry {
                path: arena.full_path(index, '\\'),
                size: arena.size_bytes(index),
                is_dir: arena.is_dir(index),
            });
        }

        let regex = match query {
            Query::Regex(pattern) => Regex::builder()
                .syntax(syntax::Config::new().case_insensitive(true))
                .build(pattern)
                .ok(),
            _ => None,
        };
        let substring_lower = match query {
            Query::Substring(needle) => Some(needle.to_ascii_lowercase()),
            _ => None,
        };
        for (index, record) in self.delta.live_added() {
            let matches = match query {
                Query::Prefix(prefix) => {
                    ascii::starts_with_ci(record.name.as_bytes(), prefix.as_bytes())
                }
                Query::Substring(_) => ascii::contains_ci(
                    record.name.as_bytes(),
                    substring_lower.as_ref().unwrap().as_bytes(),
                ),
                Query::Regex(_) => regex
                    .as_ref()
                    .is_some_and(|compiled| compiled.is_match(record.name.as_bytes())),
            };
            if matches {
                entries.push(ResultEntry {
                    path: self.delta_path(index),
                    size: record.size_bytes,
                    is_dir: record.is_dir,
                });
            }
        }
        entries.truncate(limit);
        entries
    }

    pub fn delta_path(&self, mut index: u32) -> String {
        let mut parts = Vec::new();
        let mut base_parent = None;
        for _ in 0..512 {
            let record = &self.delta.added[index as usize];
            parts.push(record.name.clone());
            match record.parent {
                ParentRef::Base(parent) => {
                    base_parent = Some(parent);
                    break;
                }
                ParentRef::Delta(parent) => index = parent,
                ParentRef::None => break,
            }
        }
        parts.reverse();
        let suffix = parts.join("\\");
        match base_parent {
            Some(parent) => format!("{}\\{suffix}", self.base.archived().full_path(parent, '\\')),
            None => suffix,
        }
    }

    /// Merge the overlay into a new base without re-enumerating the volume.
    pub fn compact(&self) -> (Arena, Vec<FrnEntry>) {
        let arena = self.base.archived();
        let mut frns_by_base = vec![None; arena.len()];
        if let Some(map) = &self.base.frn_map {
            for entry in map.iter() {
                if let Some(slot) = frns_by_base.get_mut(entry.index as usize) {
                    *slot = Some(entry.frn);
                }
            }
        }
        let mut frns_by_delta = vec![None; self.delta.added.len()];
        for (&frn, &index) in &self.delta.added_frns {
            frns_by_delta[index as usize] = Some(frn);
        }

        let live_base = arena.len() - self.delta.tombstones.count_ones() as usize;
        let live_delta = self.delta.live_added().count();
        let mut builder = ArenaBuilder::with_capacity(live_base + live_delta, arena.names.len());
        let mut base_indices = vec![None; arena.len()];
        let mut delta_indices = vec![None; self.delta.added.len()];
        let mut name = Vec::new();

        for old in 0..arena.len() as u32 {
            if self.delta.tombstones.get(old) {
                continue;
            }
            arena.name_into(old, &mut name);
            let new = match frns_by_base[old as usize] {
                Some(frn) => builder.push_bytes_with_metadata(
                    &name,
                    arena.mtime(old),
                    arena.is_dir(old),
                    Some(frn),
                    arena.size_bytes(old),
                ),
                None => builder.push_bytes_with_metadata(
                    &name,
                    arena.mtime(old),
                    arena.is_dir(old),
                    None,
                    arena.size_bytes(old),
                ),
            };
            base_indices[old as usize] = Some(new);
        }
        for (old, record) in self.delta.live_added() {
            let new = match frns_by_delta[old as usize] {
                Some(frn) => builder.push_bytes_with_metadata(
                    record.name.as_bytes(),
                    record.mtime_secs,
                    record.is_dir,
                    Some(frn),
                    record.size_bytes,
                ),
                None => builder.push_bytes_with_metadata(
                    record.name.as_bytes(),
                    record.mtime_secs,
                    record.is_dir,
                    None,
                    record.size_bytes,
                ),
            };
            delta_indices[old as usize] = Some(new);
        }

        for old in 0..arena.len() as u32 {
            let Some(new) = base_indices[old as usize] else {
                continue;
            };
            let parent = arena.parent(old);
            if parent != PARENT_NONE {
                if let Some(new_parent) = base_indices[parent as usize] {
                    builder.set_parent(new, new_parent);
                }
            }
        }
        for (old, record) in self.delta.live_added() {
            let new = delta_indices[old as usize].unwrap();
            let parent = match record.parent {
                ParentRef::Base(index) => base_indices[index as usize],
                ParentRef::Delta(index) => delta_indices[index as usize],
                ParentRef::None => None,
            };
            if let Some(parent) = parent {
                builder.set_parent(new, parent);
            }
        }
        builder.build()
    }
}

pub fn fresh_generation() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Search a validated archived base together with its coherent delta overlay.
pub fn search_archived_with_delta(
    arena: &crate::ArchivedArena,
    delta: &Delta,
    query: &Query,
    limit: usize,
) -> Vec<ResultEntry> {
    let base_hits = search_base(arena, query, usize::MAX);
    let mut entries = Vec::with_capacity(base_hits.len().min(limit));
    for index in base_hits {
        if delta.tombstones.get(index) {
            continue;
        }
        entries.push(ResultEntry {
            path: arena.full_path(index, '\\'),
            size: arena.size_bytes(index),
            is_dir: arena.is_dir(index),
        });
        if entries.len() >= limit {
            return entries;
        }
    }

    let compiled = match query {
        Query::Regex(pattern) => Regex::builder()
            .syntax(syntax::Config::new().case_insensitive(true))
            .build(pattern)
            .ok(),
        _ => None,
    };
    let substring_lower = match query {
        Query::Substring(needle) => Some(needle.to_ascii_lowercase()),
        _ => None,
    };
    for (index, record) in delta.live_added() {
        let matched = match query {
            Query::Prefix(prefix) => {
                ascii::starts_with_ci(record.name.as_bytes(), prefix.as_bytes())
            }
            Query::Substring(_) => ascii::contains_ci(
                record.name.as_bytes(),
                substring_lower.as_ref().unwrap().as_bytes(),
            ),
            Query::Regex(_) => compiled
                .as_ref()
                .is_some_and(|regex| regex.is_match(record.name.as_bytes())),
        };
        if matched {
            entries.push(ResultEntry {
                path: delta_path(arena, delta, index),
                size: record.size_bytes,
                is_dir: record.is_dir,
            });
            if entries.len() >= limit {
                break;
            }
        }
    }
    entries
}

fn delta_path(arena: &crate::ArchivedArena, delta: &Delta, mut index: u32) -> String {
    let mut parts = Vec::new();
    let mut base_parent = None;
    for _ in 0..512 {
        let record = &delta.added[index as usize];
        parts.push(record.name.clone());
        match record.parent {
            ParentRef::Base(parent) => {
                base_parent = Some(parent);
                break;
            }
            ParentRef::Delta(parent) => index = parent,
            ParentRef::None => break,
        }
    }
    parts.reverse();
    let suffix = parts.join("\\");
    match base_parent {
        Some(parent) => format!("{}\\{suffix}", arena.full_path(parent, '\\')),
        None => suffix,
    }
}
