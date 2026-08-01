use std::sync::Arc;

use regex_automata::meta::Regex;
use regex_automata::util::syntax;

use crate::ascii;
use crate::delta::{Delta, ParentRef};
use crate::protocol::ResultEntry;
use crate::query::{search_base, Query};
use crate::store::ArenaStore;

/// Immutable base-and-overlay pair published through one atomic pointer.
pub struct IndexView {
    pub base: Arc<ArenaStore>,
    pub delta: Arc<Delta>,
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
            live: true,
        });
        delta.added.push(DeltaRecord {
            name: "y".into(),
            parent: ParentRef::Delta(0),
            mtime_secs: 0,
            is_dir: false,
            live: true,
        });
        view.delta = Arc::new(delta);
        assert!(view.delta_path(1).ends_with("X\\y"));
    }
}

impl IndexView {
    pub fn new(base: Arc<ArenaStore>) -> Self {
        let records = base.archived().len();
        Self {
            base,
            delta: Arc::new(Delta::new(records)),
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
            let record = &arena.records[index as usize];
            entries.push(ResultEntry {
                path: arena.full_path(index, '\\'),
                size: 0,
                is_dir: record.is_dir(),
            });
        }

        let regex = match query {
            Query::Regex(pattern) => Regex::builder()
                .syntax(syntax::Config::new().case_insensitive(true))
                .build(pattern)
                .ok(),
            _ => None,
        };
        for (index, record) in self.delta.live_added() {
            let matches = match query {
                Query::Prefix(prefix) => {
                    ascii::starts_with_ci(record.name.as_bytes(), prefix.as_bytes())
                }
                Query::Substring(needle) => {
                    ascii::contains_ci(record.name.as_bytes(), needle.as_bytes())
                }
                Query::Regex(_) => regex
                    .as_ref()
                    .is_some_and(|compiled| compiled.is_match(record.name.as_bytes())),
            };
            if matches {
                entries.push(ResultEntry {
                    path: self.delta_path(index),
                    size: 0,
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
}
