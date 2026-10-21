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
mod file_backed;
pub use file_backed::{build_file_backed, prefix_sums_u64_file_backed, FileBackedDfsLayout};

/// Depth-first order over a parent column. See the module docs.
pub struct DfsLayout {
    pub positions: Vec<u32>,
    pub records: Vec<u32>,
    pub subtree_ends: Vec<u32>,
}

/// Prefix sums of a per-record `u32` column, laid out in depth-first order.
///
/// `prefix[k]` is the sum of `values[records[0..k]]`, so `prefix.len() ==
/// records.len() + 1` and a subtree total over `positions[r]..subtree_ends[r]`
/// is one subtraction: `prefix[subtree_ends[r]] - prefix[positions[r]]`. A
/// leaf's own interval has width one, so the same formula gives back its own
/// value — callers do not need an `is_dir` branch.
///
/// Sums accumulate in `u64` even though each per-record value is a saturating
/// `u32` (KiB): a directory's recursive total can exceed `u32::MAX` KiB (~4
/// TiB) on a large volume even though no single file can, and the running sum
/// must not wrap partway through the corpus.
#[inline]
pub fn prefix_sums_u64(records: &[u32], values: &[u32]) -> Vec<u64> {
    let mut prefix = Vec::with_capacity(records.len() + 1);
    prefix.push(0u64);
    let mut running = 0u64;
    for &record in records {
        running += values[record as usize] as u64;
        prefix.push(running);
    }
    prefix
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

// ── file-backed twin, for compaction ─────────────────────────────────────

#[cfg(test)]
mod tests;
