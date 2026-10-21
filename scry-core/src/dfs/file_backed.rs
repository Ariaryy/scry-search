//! Spool-backed DFS traversal and recursive-size prefix sums.

use crate::record::{unpack_parent, PARENT_NONE};
use crate::spool::{Pod, Spool};
use std::fs::File;
use std::io;
use std::path::Path;

/// Spool-backed twin of [`super::DfsLayout`]. `build_file_backed` produces exactly
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

/// File-backed twin of [`super::build`]. `parents` may be backed by anything that
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

/// File-backed twin of [`super::prefix_sums_u64`]: same running-sum pass, output in
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
