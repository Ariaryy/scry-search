//! Owned construction from unsorted input records.

use crate::ascii;
use crate::frnmap::FrnEntry;
use crate::record::{bytes_to_size_kib, pack_parent, PARENT_NONE};
use crate::trigram::{num_blocks, row_bytes, TRIGRAM_BLOCK, TRIGRAM_ROWS};

use super::names::front_code;
use super::{derive_size_exact_bits, Arena};

#[derive(Default)]
pub struct ArenaBuilder {
    /// All names concatenated, no per-name allocation. Sorting operates on a
    /// permutation of indices into `name_ends`, comparing slices of this blob
    /// directly — the previous `Vec<String>` cost one heap allocation per
    /// filename and made a full rebuild the dominant source of the daemon's
    /// memory spikes.
    staging_names: Vec<u8>,
    /// Exclusive end offset of each name in `staging_names`; the start of
    /// name `i` is `name_ends[i-1]` (or 0 for `i == 0`).
    name_ends: Vec<u32>,
    parents: Vec<u32>,
    mtimes: Vec<u32>,
    dirs: Vec<bool>,
    sizes: Vec<u64>,
    size_exact: Vec<bool>,
    frns: Vec<Option<u64>>,
    journal_id: u64,
    next_usn: i64,
    volume_serial: u64,
}

impl ArenaBuilder {
    /// Reserve capacity up front. Callers that know the entry count should
    /// use this — growth reallocation of the staging blob is the largest
    /// single transient allocation in a rebuild.
    pub fn with_capacity(entries: usize, name_bytes: usize) -> Self {
        ArenaBuilder {
            staging_names: Vec::with_capacity(name_bytes),
            name_ends: Vec::with_capacity(entries),
            parents: Vec::with_capacity(entries),
            mtimes: Vec::with_capacity(entries),
            dirs: Vec::with_capacity(entries),
            sizes: Vec::with_capacity(entries),
            size_exact: Vec::with_capacity(entries),
            frns: Vec::with_capacity(entries),
            journal_id: 0,
            next_usn: 0,
            volume_serial: 0,
        }
    }

    /// Push an entry whose name is given as UTF-8 bytes, copied into the
    /// staging blob. Returns its provisional index (valid for `set_parent`
    /// calls before `build()` — `build()` reorders into name order and
    /// rewrites all parent links).
    pub fn push_bytes(&mut self, name: &[u8], mtime_secs: u32, is_dir: bool) -> u32 {
        self.push_bytes_internal(name, mtime_secs, is_dir, None)
    }

    pub fn push_bytes_with_frn(
        &mut self,
        name: &[u8],
        mtime_secs: u32,
        is_dir: bool,
        frn: u64,
    ) -> u32 {
        self.push_bytes_internal(name, mtime_secs, is_dir, Some(frn))
    }

    fn push_bytes_internal(
        &mut self,
        name: &[u8],
        mtime_secs: u32,
        is_dir: bool,
        frn: Option<u64>,
    ) -> u32 {
        self.push_bytes_with_metadata(name, mtime_secs, is_dir, frn, 0)
    }

    pub fn push_bytes_with_metadata(
        &mut self,
        name: &[u8],
        mtime_secs: u32,
        is_dir: bool,
        frn: Option<u64>,
        size_bytes: u64,
    ) -> u32 {
        self.push_bytes_with_metadata_exact(name, mtime_secs, is_dir, frn, size_bytes, true)
    }

    pub fn push_bytes_with_metadata_exact(
        &mut self,
        name: &[u8],
        mtime_secs: u32,
        is_dir: bool,
        frn: Option<u64>,
        size_bytes: u64,
        size_exact: bool,
    ) -> u32 {
        let idx = self.name_ends.len() as u32;
        self.staging_names.extend_from_slice(name);
        self.name_ends.push(self.staging_names.len() as u32);
        self.parents.push(PARENT_NONE);
        self.mtimes.push(mtime_secs);
        self.dirs.push(is_dir);
        self.frns.push(frn);
        self.sizes.push(size_bytes);
        self.size_exact.push(size_exact);
        idx
    }

    /// Convenience wrapper for callers that already have a `&str` (tests,
    /// mostly). Prefer `push_bytes` on the hot path.
    pub fn push(&mut self, name: &str, mtime_secs: u32, is_dir: bool) -> u32 {
        self.push_bytes(name.as_bytes(), mtime_secs, is_dir)
    }

    pub fn set_parent(&mut self, idx: u32, parent: u32) {
        self.parents[idx as usize] = parent;
    }

    /// Stamps the journal cursor captured before enumeration.
    pub fn set_snapshot_cursor(&mut self, journal_id: u64, next_usn: i64, volume_serial: u64) {
        self.journal_id = journal_id;
        self.next_usn = next_usn;
        self.volume_serial = volume_serial;
    }

    pub fn len(&self) -> usize {
        self.name_ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.name_ends.is_empty()
    }

    fn staged_name(&self, i: u32) -> &[u8] {
        let i = i as usize;
        let start = if i == 0 {
            0
        } else {
            self.name_ends[i - 1] as usize
        };
        let end = self.name_ends[i] as usize;
        &self.staging_names[start..end]
    }

    /// Finalize: sort by name, remap parents, front-code names.
    pub fn build(mut self) -> (Arena, Vec<FrnEntry>) {
        let n = self.name_ends.len();
        assert!(
            n < PARENT_NONE as usize,
            "arena exceeds maximum capacity ({} entries, limit {})",
            n,
            PARENT_NONE
        );

        // 1. Build sort order: ASCII-case-insensitive, ties by insertion index.
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_unstable_by(|&a, &b| {
            ascii::cmp_ci(self.staged_name(a), self.staged_name(b)).then(a.cmp(&b))
        });

        // 2. Build inverse permutation: rank[order[j]] = j.
        let mut rank = vec![0u32; n];
        for (j, &orig) in order.iter().enumerate() {
            rank[orig as usize] = j as u32;
        }

        // 3. Build sorted columns with remapped parents.
        let mut out_parents: Vec<u32> = Vec::with_capacity(n);
        let mut out_mtimes: Vec<u32> = Vec::with_capacity(n);
        let mut out_sizes: Vec<u32> = Vec::with_capacity(n);
        let mut out_size_exact = Vec::with_capacity(n);
        for &orig in &order {
            let orig_parent = self.parents[orig as usize];
            let new_parent = if orig_parent == PARENT_NONE {
                PARENT_NONE
            } else {
                rank[orig_parent as usize]
            };
            out_parents.push(pack_parent(new_parent, self.dirs[orig as usize]));
            out_mtimes.push(self.mtimes[orig as usize]);
            out_sizes.push(bytes_to_size_kib(self.sizes[orig as usize]));
            out_size_exact.push(self.size_exact[orig as usize]);
        }
        let frn_entries = order
            .iter()
            .enumerate()
            .filter_map(|(index, &original)| {
                self.frns[original as usize].map(|frn| FrnEntry {
                    frn,
                    index: index as u32,
                    _pad: 0,
                })
            })
            .collect();

        // 4. Front-code names and build the block filter in final order.
        let name_refs: Vec<&[u8]> = order.iter().map(|&i| self.staged_name(i)).collect();
        let blocks = num_blocks(n);
        let row_len = row_bytes(blocks);
        let mut trigram_index = if n < TRIGRAM_BLOCK {
            Vec::new()
        } else {
            vec![0; TRIGRAM_ROWS * row_len]
        };
        let mut names = Vec::new();
        let mut bucket_offsets = Vec::new();
        front_code(
            &name_refs,
            &mut names,
            &mut bucket_offsets,
            &mut trigram_index,
            row_len,
        );

        // The staging blob and the front-coded output blob briefly coexist
        // above; drop the staging blob before returning so it isn't held
        // alongside the (smaller, but still substantial) output for the rest
        // of the caller's stack frame. Leave this explicit — a "tidy" refactor
        // that removes it silently regresses peak build memory.
        drop(std::mem::take(&mut self.staging_names));
        drop(std::mem::take(&mut self.name_ends));

        // 5. Tree order, over the *remapped* parents — `dfs` indexes records
        // the same way every other column does.
        let dfs = crate::dfs::build(&out_parents);
        let size_exact_bits = derive_size_exact_bits(&out_parents, &dfs, &out_size_exact);
        // 6. Recursive-size prefix sums, over the same tree order and the
        // just-built size column — see the `dfs_size_prefix` doc comment.
        let dfs_size_prefix = crate::dfs::prefix_sums_u64(&dfs.records, &out_sizes);

        (
            Arena {
                format_version: crate::record::FORMAT_VERSION,
                journal_id: self.journal_id,
                next_usn: self.next_usn,
                volume_serial: self.volume_serial,
                names,
                bucket_offsets,
                parents: out_parents,
                mtimes: out_mtimes,
                sizes: out_sizes,
                size_exact_bits,
                trigram_index,
                dfs_positions: dfs.positions,
                dfs_records: dfs.records,
                dfs_ends: dfs.subtree_ends,
                dfs_size_prefix,
            },
            frn_entries,
        )
    }
}
