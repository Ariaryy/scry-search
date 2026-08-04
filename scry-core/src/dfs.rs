//! Depth-first subtree intervals over the name-sorted record set.
//!
//! Records stay in name-sorted order — `prefix_range`'s binary search and the
//! front-coded name blob both depend on it — so subtree membership cannot be
//! read off the record index. These three parallel arrays add a second,
//! *tree* order alongside it:
//!
//! - `positions[record]` — where a record sits in depth-first order.
//! - `records[position]` — the inverse, so a span of tree order enumerates
//!   back to record indices.
//! - `subtree_ends[record]` — exclusive end, in tree-order positions, of the
//!   subtree rooted at that record.
//!
//! Together they turn "every record beneath directory `d`" — an ancestor walk
//! per record today, which is why the path-terms scan has to touch all of them
//! — into the half-open interval `positions[d] .. subtree_ends[d]`. Descendant
//! *count* falls out as the width of that interval without visiting anything,
//! and a prefix sum over any per-record column laid out in tree order would
//! make recursive aggregates over that column O(1) per directory.
//!
//! The cost is twelve bytes per record in the snapshot. They live in the
//! mmapped archive rather than on the heap, so a query that never consults
//! them never pages them in.
//!
//! The parent graph comes off a live volume and is not trusted to be a forest:
//! it may contain cycles, self-parents and dangling indices. Every record
//! still receives exactly one position — cycle members are entered as if they
//! were roots — so `positions` is always a permutation and callers can index
//! it without a bounds story.

use crate::record::{unpack_parent, PARENT_NONE};

/// Depth-first order over a parent column. See the module docs.
pub struct DfsLayout {
    pub positions: Vec<u32>,
    pub records: Vec<u32>,
    pub subtree_ends: Vec<u32>,
}

/// Children of each record, as a compressed sparse row over record indices.
///
/// Built and dropped inside [`build`] rather than stored: it is only needed to
/// walk the tree once, and at three words per record it would otherwise be the
/// largest transient in a rebuild after the staging name blob.
struct ChildTable {
    starts: Vec<u32>,
    children: Vec<u32>,
}

impl ChildTable {
    fn build(parents: &[u32]) -> Self {
        let n = parents.len();
        let mut starts = vec![0u32; n + 1];
        for (child, &word) in parents.iter().enumerate() {
            let parent = unpack_parent(word) as usize;
            // A self-parent would make the record its own child and spin the
            // traversal; a dangling index has no row to be counted into.
            if parent < n && parent != child {
                starts[parent + 1] += 1;
            }
        }
        for index in 1..=n {
            starts[index] += starts[index - 1];
        }
        let mut children = vec![0u32; starts[n] as usize];
        // `cursor` walks each row as it fills; `starts` must survive intact.
        let mut cursor = starts.clone();
        for (child, &word) in parents.iter().enumerate() {
            let parent = unpack_parent(word) as usize;
            if parent < n && parent != child {
                children[cursor[parent] as usize] = child as u32;
                cursor[parent] += 1;
            }
        }
        Self { starts, children }
    }

    fn children_of(&self, record: u32) -> &[u32] {
        let start = self.starts[record as usize] as usize;
        let end = self.starts[record as usize + 1] as usize;
        &self.children[start..end]
    }
}

/// Assigns every record a depth-first position and subtree interval.
///
/// Roots are entered in record order, then any record left unvisited — which
/// can only be a member of a parent cycle — is entered as its own root, so the
/// result is a permutation even on a malformed parent column.
pub fn build(parents: &[u32]) -> DfsLayout {
    let n = parents.len();
    if n == 0 {
        return DfsLayout {
            positions: Vec::new(),
            records: Vec::new(),
            subtree_ends: Vec::new(),
        };
    }
    // The child table peaks at three words per record while it fills, and the
    // three output columns are another three. Building the table first, and
    // only then allocating the outputs, keeps those two peaks from adding:
    // twenty bytes per record instead of twenty-four, which on a
    // two-million-record volume is the difference between 40 MB and 48 MB of
    // transient. Do not hoist the allocations back above this line.
    let table = ChildTable::build(parents);
    let mut layout = DfsLayout {
        positions: vec![0u32; n],
        records: Vec::with_capacity(n),
        subtree_ends: vec![0u32; n],
    };
    let mut visited = vec![0u8; n.div_ceil(8)];
    // (record, index of the next child to descend into). Explicit rather than
    // recursive: a volume with a deep tree — or a long parent chain from a
    // corrupt column — would otherwise overflow the stack.
    let mut stack: Vec<(u32, u32)> = Vec::new();

    for start in 0..n as u32 {
        let parent = unpack_parent(parents[start as usize]);
        let is_root = parent == PARENT_NONE || parent as usize >= n || parent == start;
        if !is_root {
            continue;
        }
        walk(start, &table, &mut visited, &mut stack, &mut layout);
    }
    // Anything still unvisited is inside a cycle and unreachable from a root.
    for start in 0..n as u32 {
        if !is_visited(&visited, start) {
            walk(start, &table, &mut visited, &mut stack, &mut layout);
        }
    }
    debug_assert_eq!(layout.records.len(), n, "every record receives a position");
    layout
}

fn walk(
    start: u32,
    table: &ChildTable,
    visited: &mut [u8],
    stack: &mut Vec<(u32, u32)>,
    layout: &mut DfsLayout,
) {
    if is_visited(visited, start) {
        return;
    }
    mark_visited(visited, start);
    layout.positions[start as usize] = layout.records.len() as u32;
    layout.records.push(start);
    stack.clear();
    stack.push((start, 0));

    while let Some((record, next_child)) = stack.last_mut() {
        let record = *record;
        let children = table.children_of(record);
        let mut descended = false;
        while (*next_child as usize) < children.len() {
            let child = children[*next_child as usize];
            *next_child += 1;
            // A cycle re-enters a record already on the stack; skipping it
            // keeps the traversal finite and leaves the record with the
            // position it was first given.
            if is_visited(visited, child) {
                continue;
            }
            mark_visited(visited, child);
            layout.positions[child as usize] = layout.records.len() as u32;
            layout.records.push(child);
            stack.push((child, 0));
            descended = true;
            break;
        }
        if !descended {
            layout.subtree_ends[record as usize] = layout.records.len() as u32;
            stack.pop();
        }
    }
}

#[inline]
fn is_visited(visited: &[u8], record: u32) -> bool {
    visited[record as usize / 8] & (1 << (record % 8)) != 0
}

#[inline]
fn mark_visited(visited: &mut [u8], record: u32) {
    visited[record as usize / 8] |= 1 << (record % 8);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::pack_parent;

    /// `positions` must be a bijection onto `0..n` and agree with `records`,
    /// or an interval test would silently address the wrong record.
    fn assert_is_permutation(layout: &DfsLayout, n: usize) {
        assert_eq!(layout.records.len(), n);
        let mut seen = vec![false; n];
        for (position, &record) in layout.records.iter().enumerate() {
            assert!(!seen[record as usize], "record {record} placed twice");
            seen[record as usize] = true;
            assert_eq!(layout.positions[record as usize], position as u32);
        }
        assert!(seen.into_iter().all(|hit| hit), "a record got no position");
    }

    /// The defining property: a record is a descendant of `d` exactly when its
    /// position lies in `d`'s interval. Checked against an independent
    /// ancestor walk over the same parent column.
    fn assert_intervals_match_ancestor_walk(parents: &[u32], layout: &DfsLayout) {
        let n = parents.len();
        for ancestor in 0..n as u32 {
            let span = layout.positions[ancestor as usize]..layout.subtree_ends[ancestor as usize];
            for record in 0..n as u32 {
                let mut current = record;
                let mut is_descendant = false;
                for _ in 0..=n {
                    if current == ancestor {
                        is_descendant = true;
                        break;
                    }
                    let parent = unpack_parent(parents[current as usize]);
                    if parent == PARENT_NONE || parent as usize >= n || parent == current {
                        break;
                    }
                    current = parent;
                }
                let in_span = span.contains(&layout.positions[record as usize]);
                if is_descendant {
                    assert!(
                        in_span,
                        "record {record} descends from {ancestor} but sits outside its interval"
                    );
                }
                // The converse only holds for a well-formed forest; a cycle
                // member entered as a pseudo-root legitimately sits inside an
                // interval it does not descend from.
            }
        }
    }

    fn forest(edges: &[(u32, bool)]) -> Vec<u32> {
        edges
            .iter()
            .map(|&(parent, is_dir)| pack_parent(parent, is_dir))
            .collect()
    }

    #[test]
    fn empty_parent_column_produces_an_empty_layout() {
        let layout = build(&[]);
        assert!(layout.records.is_empty());
        assert!(layout.positions.is_empty());
    }

    #[test]
    fn subtree_interval_covers_exactly_the_descendants() {
        // 0 root, 1 and 2 children of 0, 3 and 4 children of 1.
        let parents = forest(&[
            (PARENT_NONE, true),
            (0, true),
            (0, false),
            (1, false),
            (1, false),
        ]);
        let layout = build(&parents);
        assert_is_permutation(&layout, parents.len());
        assert_intervals_match_ancestor_walk(&parents, &layout);

        let root_span = layout.positions[0]..layout.subtree_ends[0];
        assert_eq!(root_span.len(), 5, "root's subtree is the whole forest");
        let one_span = layout.positions[1]..layout.subtree_ends[1];
        assert_eq!(one_span.len(), 3, "1 plus its two children");
        let leaf_span = layout.positions[2]..layout.subtree_ends[2];
        assert_eq!(leaf_span.len(), 1, "a leaf's subtree is itself");
    }

    #[test]
    fn descendant_count_is_the_interval_width() {
        let parents = forest(&[
            (PARENT_NONE, true),
            (0, true),
            (1, true),
            (2, false),
            (0, false),
        ]);
        let layout = build(&parents);
        for (record, expected) in [(0u32, 5u32), (1, 3), (2, 2), (3, 1), (4, 1)] {
            assert_eq!(
                layout.subtree_ends[record as usize] - layout.positions[record as usize],
                expected,
                "record {record}"
            );
        }
    }

    #[test]
    fn a_parent_cycle_still_yields_a_permutation() {
        // 0 -> 1 -> 2 -> 0, plus an unrelated root with a child.
        let parents = forest(&[
            (1, true),
            (2, true),
            (0, true),
            (PARENT_NONE, true),
            (3, false),
        ]);
        let layout = build(&parents);
        assert_is_permutation(&layout, parents.len());
    }

    #[test]
    fn a_self_parent_is_treated_as_a_root() {
        let parents = forest(&[(0, true), (0, false)]);
        let layout = build(&parents);
        assert_is_permutation(&layout, parents.len());
        assert_eq!(layout.subtree_ends[0] - layout.positions[0], 2);
    }

    #[test]
    fn a_dangling_parent_index_is_treated_as_a_root() {
        let parents = forest(&[(9_999, true), (0, false)]);
        let layout = build(&parents);
        assert_is_permutation(&layout, parents.len());
        assert_intervals_match_ancestor_walk(&parents, &layout);
    }

    /// A long chain must not recurse — this is the shape that would overflow
    /// the stack if `walk` were written recursively.
    #[test]
    fn a_deep_chain_does_not_overflow_the_stack() {
        const DEPTH: u32 = 200_000;
        let mut parents = vec![pack_parent(PARENT_NONE, true)];
        for index in 1..DEPTH {
            parents.push(pack_parent(index - 1, true));
        }
        let layout = build(&parents);
        assert_is_permutation(&layout, parents.len());
        assert_eq!(layout.subtree_ends[0] - layout.positions[0], DEPTH);
        assert_eq!(layout.subtree_ends[(DEPTH - 1) as usize], DEPTH);
    }

    /// Randomised forests, checked against the independent ancestor walk.
    #[test]
    fn random_forests_match_an_ancestor_walk() {
        let mut state = 0x243F_6A88_85A3_08D3u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200 {
            let n = 2 + (next() % 60) as usize;
            let parents: Vec<u32> = (0..n)
                .map(|index| {
                    if index == 0 || next() % 4 == 0 {
                        pack_parent(PARENT_NONE, true)
                    } else {
                        // Parent strictly earlier, so the graph stays acyclic
                        // and the converse of the interval property holds.
                        pack_parent((next() % index as u64) as u32, next() % 2 == 0)
                    }
                })
                .collect();
            let layout = build(&parents);
            assert_is_permutation(&layout, n);
            assert_intervals_match_ancestor_walk(&parents, &layout);
        }
    }
}
