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
use crate::spool::{Pod, Spool};
use std::fs::File;
use std::io;
use std::path::Path;

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

/// Spool-backed twin of [`DfsLayout`]. `build_file_backed` produces exactly
/// the same positions/records/subtree_ends `build` would from the same
/// parent column — the traversal, the cycle/self-parent/dangling-parent
/// handling, and the two-pass root-then-orphan order are all unchanged; only
/// the scratch and the outputs move from `Vec` to a mmap-backed [`Spool`], so
/// a two-million-record compaction pays a handful of resident pages for this
/// pass instead of ~40 MB of heap (the child table, the three outputs, the
/// visited bitset, and the worst-case traversal stack, which — unlike a
/// recursion depth — is bounded by tree depth and can approach one entry per
/// record on a corrupt or pathologically deep parent chain).
///
/// `build` (and `ArenaBuilder`'s full-rebuild path, which calls it) is
/// unchanged and untouched: this is a separate function for compaction's
/// second pass specifically, not a replacement.
pub struct FileBackedDfsLayout {
    pub positions: Spool<u32>,
    pub records: Spool<u32>,
    pub subtree_ends: Spool<u32>,
}

/// A traversal-stack frame: which record, and the index of the next child of
/// that record still to be visited. `#[repr(C)]`, hand-rolled `Pod` impl —
/// same pattern as `FrnEntry` — rather than trusting an un-`repr`'d tuple's
/// layout to a raw byte reinterpretation.
#[repr(C)]
#[derive(Clone, Copy)]
struct StackFrame {
    record: u32,
    next_child: u32,
}
unsafe impl Pod for StackFrame {}

struct FileBackedChildTable {
    starts: Spool<u32>,
    children: Spool<u32>,
}

impl FileBackedChildTable {
    fn build(parents: &[u32], dir: &Path, on_create: &impl Fn(&File)) -> io::Result<Self> {
        let n = parents.len();
        let mut starts: Spool<u32> =
            Spool::zeroed(&dir.join("dfs-starts.spool"), n + 1, |f| on_create(f))?;
        for (child, &word) in parents.iter().enumerate() {
            let parent = unpack_parent(word) as usize;
            if parent < n && parent != child {
                let count = starts.get(parent + 1);
                starts.set(parent + 1, count + 1);
            }
        }
        for index in 1..=n {
            let prev = starts.get(index - 1);
            let here = starts.get(index);
            starts.set(index, prev + here);
        }
        let total_children = starts.get(n) as usize;
        let mut children: Spool<u32> = Spool::zeroed(
            &dir.join("dfs-children.spool"),
            total_children.max(1),
            |f| on_create(f),
        )?;
        // A cursor walking each row as it fills, distinct from `starts`
        // (which must survive intact for later `children_range` lookups) —
        // same two-array shape `ChildTable::build` uses, just spool-backed.
        let mut cursor: Spool<u32> =
            Spool::create(&dir.join("dfs-cursor.spool"), n + 1, |f| on_create(f))?;
        for i in 0..=n {
            cursor.push(starts.get(i));
        }
        for (child, &word) in parents.iter().enumerate() {
            let parent = unpack_parent(word) as usize;
            if parent < n && parent != child {
                let at = cursor.get(parent);
                children.set(at as usize, child as u32);
                cursor.set(parent, at + 1);
            }
        }
        drop(cursor);
        Ok(Self { starts, children })
    }

    fn children_range(&self, record: u32) -> std::ops::Range<usize> {
        let start = self.starts.get(record as usize) as usize;
        let end = self.starts.get(record as usize + 1) as usize;
        start..end
    }
}

#[inline]
fn is_visited_spool(visited: &Spool<u8>, record: u32) -> bool {
    visited.get(record as usize / 8) & (1 << (record % 8)) != 0
}

#[inline]
fn mark_visited_spool(visited: &mut Spool<u8>, record: u32) {
    let byte = visited.get(record as usize / 8);
    visited.set(record as usize / 8, byte | (1 << (record % 8)));
}

/// File-backed twin of [`build`]. `parents` may be backed by anything that
/// derefs to a slice, including a `Spool<u32>`'s [`Spool::as_slice`] — the
/// parent column itself does not need to move off the heap for this function
/// to avoid allocating one, since compaction's own parent spool already
/// lives on disk before this runs.
///
/// `dir` is a scratch directory for this call's spool files; `on_create` runs
/// against every one of them so a caller (compaction) can mark them
/// auxiliary before they can generate a watched USN event, exactly like every
/// other temp file compaction creates.
pub fn build_file_backed(
    parents: &[u32],
    dir: &Path,
    on_create: &impl Fn(&File),
) -> io::Result<FileBackedDfsLayout> {
    let n = parents.len();
    if n == 0 {
        return Ok(FileBackedDfsLayout {
            positions: Spool::create(&dir.join("dfs-positions.spool"), 0, |f| on_create(f))?,
            records: Spool::create(&dir.join("dfs-records.spool"), 0, |f| on_create(f))?,
            subtree_ends: Spool::create(&dir.join("dfs-subtree-ends.spool"), 0, |f| on_create(f))?,
        });
    }
    // Same ordering rationale as `build`: the child table peaks first and is
    // dropped before the three output columns are reserved, keeping the two
    // peaks from summing — it matters less here since both live off-heap,
    // but there is no reason to give that up.
    let table = FileBackedChildTable::build(parents, dir, on_create)?;
    let mut layout = FileBackedDfsLayout {
        positions: Spool::zeroed(&dir.join("dfs-positions.spool"), n, |f| on_create(f))?,
        records: Spool::create(&dir.join("dfs-records.spool"), n, |f| on_create(f))?,
        subtree_ends: Spool::zeroed(&dir.join("dfs-subtree-ends.spool"), n, |f| on_create(f))?,
    };
    let mut visited: Spool<u8> =
        Spool::zeroed(&dir.join("dfs-visited.spool"), n.div_ceil(8), |f| {
            on_create(f)
        })?;
    let mut stack: Spool<StackFrame> =
        Spool::create(&dir.join("dfs-stack.spool"), n, |f| on_create(f))?;

    for start in 0..n as u32 {
        let parent = unpack_parent(parents[start as usize]);
        let is_root = parent == PARENT_NONE || parent as usize >= n || parent == start;
        if !is_root {
            continue;
        }
        walk_file_backed(start, &table, &mut visited, &mut stack, &mut layout);
    }
    for start in 0..n as u32 {
        if !is_visited_spool(&visited, start) {
            walk_file_backed(start, &table, &mut visited, &mut stack, &mut layout);
        }
    }
    debug_assert_eq!(layout.records.len(), n, "every record receives a position");
    Ok(layout)
}

fn walk_file_backed(
    start: u32,
    table: &FileBackedChildTable,
    visited: &mut Spool<u8>,
    stack: &mut Spool<StackFrame>,
    layout: &mut FileBackedDfsLayout,
) {
    if is_visited_spool(visited, start) {
        return;
    }
    mark_visited_spool(visited, start);
    layout
        .positions
        .set(start as usize, layout.records.len() as u32);
    layout.records.push(start);
    stack.clear();
    stack.push(StackFrame {
        record: start,
        next_child: 0,
    });

    while let Some(frame) = stack.last() {
        let record = frame.record;
        let mut next_child = frame.next_child;
        let range = table.children_range(record);
        let mut descended = false;
        while (next_child as usize) < range.len() {
            let child = table.children.get(range.start + next_child as usize);
            next_child += 1;
            // A cycle re-enters a record already on the stack; skipping it
            // keeps the traversal finite and leaves the record with the
            // position it was first given.
            if is_visited_spool(visited, child) {
                continue;
            }
            mark_visited_spool(visited, child);
            layout
                .positions
                .set(child as usize, layout.records.len() as u32);
            layout.records.push(child);
            stack.set_last(StackFrame { record, next_child });
            stack.push(StackFrame {
                record: child,
                next_child: 0,
            });
            descended = true;
            break;
        }
        if !descended {
            layout
                .subtree_ends
                .set(record as usize, layout.records.len() as u32);
            stack.pop();
        }
    }
}

/// File-backed twin of [`prefix_sums_u64`]: same running-sum pass, output in
/// a `Spool<u64>` instead of a `Vec<u64>`.
pub fn prefix_sums_u64_file_backed(
    records: &Spool<u32>,
    sizes: &Spool<u32>,
    path: &Path,
    on_create: &impl Fn(&File),
) -> io::Result<Spool<u64>> {
    let mut prefix: Spool<u64> = Spool::create(path, records.len() + 1, |f| on_create(f))?;
    prefix.push(0u64);
    let mut running = 0u64;
    for i in 0..records.len() {
        let record = records.get(i);
        running += sizes.get(record as usize) as u64;
        prefix.push(running);
    }
    Ok(prefix)
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

    /// Recursive size over nested directories: a subtree total must equal
    /// the sum of every record beneath it, itself included.
    #[test]
    fn prefix_sums_give_correct_subtree_totals_for_nested_dirs() {
        // 0 root, 1 and 2 children of 0, 3 and 4 children of 1.
        let parents = forest(&[
            (PARENT_NONE, true),
            (0, true),
            (0, false),
            (1, false),
            (1, false),
        ]);
        let sizes = [0u32, 0, 5, 3, 7]; // dirs carry no own size in this model
        let layout = build(&parents);
        let prefix = prefix_sums_u64(&layout.records, &sizes);
        assert_eq!(prefix.len(), parents.len() + 1);

        let subtree_total = |record: usize| {
            prefix[layout.subtree_ends[record] as usize] - prefix[layout.positions[record] as usize]
        };
        assert_eq!(subtree_total(0), 15, "root covers every record");
        assert_eq!(subtree_total(1), 10, "record 1 plus its two children");
        assert_eq!(subtree_total(2), 5, "a leaf's subtree is itself");
        assert_eq!(subtree_total(3), 3);
        assert_eq!(subtree_total(4), 7);
    }

    /// A parent cycle still yields a total permutation, so the prefix-sum
    /// invariant (subtree total is a valid, in-bounds subtraction) must keep
    /// holding even when cycle members are pseudo-roots.
    #[test]
    fn prefix_sums_stay_valid_across_a_parent_cycle() {
        // 0 -> 1 -> 2 -> 0, plus an unrelated root with a child.
        let parents = forest(&[
            (1, true),
            (2, true),
            (0, true),
            (PARENT_NONE, true),
            (3, false),
        ]);
        let sizes = [1u32, 2, 3, 4, 5];
        let layout = build(&parents);
        assert_is_permutation(&layout, parents.len());
        let prefix = prefix_sums_u64(&layout.records, &sizes);
        assert_eq!(prefix.len(), parents.len() + 1);
        // Total across the whole tree order must equal the sum of every value,
        // regardless of how cycle members were rooted.
        assert_eq!(
            *prefix.last().unwrap(),
            sizes.iter().map(|&s| s as u64).sum::<u64>()
        );
        for record in 0..parents.len() {
            let total = prefix[layout.subtree_ends[record] as usize]
                - prefix[layout.positions[record] as usize];
            // Every record's own value is included in its own subtree total,
            // cycle member or not.
            assert!(total >= sizes[record] as u64);
        }
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

    fn assert_file_backed_matches(parents: &[u32]) {
        let dir = tempfile::tempdir().unwrap();
        let expected = build(parents);
        let actual = build_file_backed(parents, dir.path(), &|_: &File| {}).unwrap();
        assert_eq!(actual.positions.as_slice(), expected.positions.as_slice());
        assert_eq!(actual.records.as_slice(), expected.records.as_slice());
        assert_eq!(
            actual.subtree_ends.as_slice(),
            expected.subtree_ends.as_slice()
        );
    }

    #[test]
    fn file_backed_matches_the_in_memory_builder_on_an_empty_column() {
        assert_file_backed_matches(&[]);
    }

    #[test]
    fn file_backed_matches_the_in_memory_builder_on_a_nested_forest() {
        let parents = forest(&[
            (PARENT_NONE, true),
            (0, true),
            (0, false),
            (1, false),
            (1, false),
        ]);
        assert_file_backed_matches(&parents);
    }

    #[test]
    fn file_backed_matches_the_in_memory_builder_across_a_parent_cycle() {
        let parents = forest(&[
            (1, true),
            (2, true),
            (0, true),
            (PARENT_NONE, true),
            (3, false),
        ]);
        assert_file_backed_matches(&parents);
    }

    #[test]
    fn file_backed_matches_the_in_memory_builder_with_a_self_parent_and_a_dangling_parent() {
        assert_file_backed_matches(&forest(&[(0, true), (0, false)]));
        assert_file_backed_matches(&forest(&[(9_999, true), (0, false)]));
    }

    /// Same shape as `a_deep_chain_does_not_overflow_the_stack`: the
    /// file-backed traversal must stay iterative too.
    #[test]
    fn file_backed_handles_a_deep_chain_without_overflow() {
        const DEPTH: u32 = 200_000;
        let mut parents = vec![pack_parent(PARENT_NONE, true)];
        for index in 1..DEPTH {
            parents.push(pack_parent(index - 1, true));
        }
        assert_file_backed_matches(&parents);
    }

    #[test]
    fn file_backed_random_forests_match_the_in_memory_builder() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..50 {
            let n = 2 + (next() % 60) as usize;
            let parents: Vec<u32> = (0..n)
                .map(|index| {
                    if index == 0 || next() % 4 == 0 {
                        pack_parent(PARENT_NONE, true)
                    } else {
                        pack_parent((next() % index as u64) as u32, next() % 2 == 0)
                    }
                })
                .collect();
            assert_file_backed_matches(&parents);
        }
    }

    #[test]
    fn file_backed_prefix_sums_match_the_in_memory_version() {
        let parents = forest(&[
            (PARENT_NONE, true),
            (0, true),
            (0, false),
            (1, false),
            (1, false),
        ]);
        let sizes = [0u32, 0, 5, 3, 7];
        let dir = tempfile::tempdir().unwrap();
        let expected_layout = build(&parents);
        let expected = prefix_sums_u64(&expected_layout.records, &sizes);

        let actual_layout = build_file_backed(&parents, dir.path(), &|_: &File| {}).unwrap();
        let mut sizes_spool: Spool<u32> =
            Spool::create(&dir.path().join("sizes.spool"), sizes.len(), |_| {}).unwrap();
        for &s in &sizes {
            sizes_spool.push(s);
        }
        let actual = prefix_sums_u64_file_backed(
            &actual_layout.records,
            &sizes_spool,
            &dir.path().join("prefix.spool"),
            &|_: &File| {},
        )
        .unwrap();
        assert_eq!(actual.as_slice(), expected.as_slice());
    }
}
