//! Archive layout, canonical tree access, and mapped query walkers.

use crate::record::{unpack_parent, word_is_dir, PARENT_NONE};
use crate::trigram::{for_each_trigram, num_blocks, row_bytes};
use rkyv::{Archive, Deserialize, Serialize};

mod builder;
mod names;
mod spooled;

pub use builder::ArenaBuilder;
pub use spooled::{derive_spooled_size_exact, SpooledArenaBuilder, SpooledColumns};

/// The full index: entries in name-sorted order, names front-coded in a
/// separate blob, version-stamped for safe mmap reuse across daemon upgrades.
///
/// Parallel arrays, all plain PODs — no String, no relative pointer — so rkyv's
/// bytecheck performs a handful of bounds checks rather than chasing a pointer
/// and UTF-8-validating a name for every one of a million records.
///
/// The hot/cold split is deliberate and load-bearing:
/// - `parents` is touched by `full_path` on every displayed result. Packing it
///   alone means a 64-byte cache line holds 16 useful hops instead of 5 (as
///   it would interleaved with mtime and size).
/// - `dfs_positions` and `dfs_records` are read on every path-term query (a
///   directory match pushes `dfs_positions[r]..dfs_ends[r]` into that term's
///   interval set; the final scan maps positions back to records through
///   `dfs_records`) — hot for the same reason `parents` is.
/// - `mtimes`, `sizes` and `dfs_ends` are read only during delta metadata
///   lookup, compaction, and (for `dfs_ends`) once per matching directory
///   rather than per candidate record. Kept in their own arrays so the OS
///   can evict those pages between compaction bursts.
///
/// These arrays are index-parallel: `parents[i]`, `mtimes[i]`, `sizes[i]`,
/// `dfs_positions[i]` and `dfs_ends[i]` all describe the same entry. A future
/// change that pushes to one without the others silently corrupts the index.
/// `dfs_records` is the one exception — it is indexed by depth-first position,
/// not by record.
#[derive(Archive, Serialize, Deserialize, Debug)]
#[archive(check_bytes)]
pub struct Arena {
    /// FORMAT_VERSION constant, stamped here so `ArenaStore::open` can reject
    /// stale snapshots before any other field is read.
    pub format_version: u32,
    /// USN journal identity captured before this snapshot's enumeration.
    pub journal_id: u64,
    /// First journal position not necessarily reflected by this snapshot.
    pub next_usn: i64,
    /// Serial number of the volume that produced this snapshot.
    pub volume_serial: u64,
    /// Front-coded name blob. Names are stored in name-sorted order (matching
    /// `parents`). Decode via `bucket_offsets` + the LEB128 encoding below.
    pub names: Vec<u8>,
    /// `bucket_offsets[b]` is the byte offset of bucket `b` in `names`.
    /// `bucket_offsets[num_buckets]` == `names.len()` (sentinel).
    pub bucket_offsets: Vec<u32>,
    /// Packed parent index and directory flag, one per entry, name-sorted.
    /// Hot: walked by `full_path` on every displayed result.
    /// Bit 31 = is_dir flag; bits 0..30 = parent record index.
    pub parents: Vec<u32>,
    /// Seconds since 1970-01-01 UTC, one per entry, name-sorted.
    /// Cold: read only by delta metadata lookup and compaction, never by a
    /// query. Kept out of `parents` so it can stay paged out between bursts.
    pub mtimes: Vec<u32>,
    /// File size in KiB, rounded up and saturating at u32::MAX.
    /// Cold: same eviction rationale as `mtimes`. Consult `size_exact` to
    /// distinguish a measured empty file from an unknown size.
    pub sizes: Vec<u32>,
    /// One LSB-first exactness bit per name-sorted record.
    pub size_exact_bits: Vec<u8>,
    /// Trigram row matrix; each row has one LSB-first bit per 1024 records.
    pub trigram_index: Vec<u8>,
    /// Depth-first position of each record, indexed by name-sorted record.
    /// Hot: every path-term query reads this to build each term's interval
    /// set and to map matched positions back to records.
    /// See `crate::dfs` for what the three tree-order columns buy.
    pub dfs_positions: Vec<u32>,
    /// Inverse of `dfs_positions`: record index at each depth-first position.
    /// Hot for the same reason as `dfs_positions`.
    pub dfs_records: Vec<u32>,
    /// Exclusive end, in depth-first positions, of each record's subtree.
    /// Indexed by name-sorted record, so `dfs_positions[r]..dfs_ends[r]` is
    /// the span of `dfs_records` covering everything beneath `r`. Cold:
    /// read once per *matching directory*, not per candidate record.
    pub dfs_ends: Vec<u32>,
    /// Prefix sums of `sizes`, laid out in depth-first order (see
    /// `crate::dfs::prefix_sums_u64`). `dfs_size_prefix.len() ==
    /// dfs_records.len() + 1`. A record's recursive size is one subtraction:
    /// `dfs_size_prefix[dfs_ends[r]] - dfs_size_prefix[dfs_positions[r]]` —
    /// see `ArchivedArena::recursive_size_kib`. `u64` so the running sum
    /// cannot wrap on a multi-terabyte volume even though each per-record
    /// value is a saturating `u32` KiB count; a directory's aggregate
    /// legitimately exceeds `u32::MAX` KiB (~4 TiB) well before the volume
    /// itself does. Cold: same eviction rationale as `mtimes`/`sizes` — only
    /// `Order::Largest` reads it. Costs 8 bytes/record, i.e. ~8 MB per
    /// million records in the snapshot.
    pub dfs_size_prefix: Vec<u64>,
}

impl Arena {
    pub fn builder() -> ArenaBuilder {
        ArenaBuilder::default()
    }

    pub fn len(&self) -> usize {
        self.parents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    #[inline]
    pub fn size_exact(&self, idx: u32) -> bool {
        bitmap_get(&self.size_exact_bits, idx as usize)
    }

    /// Parent edge retained by the deterministic DFS spanning forest.
    /// Corrupt cycle/self/dangling edges that DFS had to skip read as roots.
    #[inline]
    pub fn tree_parent(&self, idx: u32) -> u32 {
        canonical_tree_parent(
            idx,
            unpack_parent(self.parents[idx as usize]),
            self.parents.len(),
            &self.dfs_positions,
            &self.dfs_ends,
        )
    }
}

#[inline]
fn canonical_tree_parent(
    idx: u32,
    parent: u32,
    len: usize,
    dfs_positions: &[u32],
    dfs_ends: &[u32],
) -> u32 {
    if parent == PARENT_NONE || parent as usize >= len || parent == idx {
        return PARENT_NONE;
    }
    // Empty DFS columns are retained only as a defensive legacy fallback.
    // A current v10 snapshot always has them when it has records.
    if dfs_positions.is_empty() || dfs_ends.is_empty() {
        return parent;
    }
    let position = dfs_positions[idx as usize];
    let parent_start = dfs_positions[parent as usize];
    let parent_end = dfs_ends[parent as usize];
    if (parent_start..parent_end).contains(&position) {
        parent
    } else {
        PARENT_NONE
    }
}

#[inline]
fn bitmap_get(bits: &[u8], index: usize) -> bool {
    bits.get(index / 8)
        .is_some_and(|byte| byte & (1 << (index % 8)) != 0)
}

#[inline]
fn bitmap_set(bits: &mut [u8], index: usize) {
    bits[index / 8] |= 1 << (index % 8);
}

fn derive_size_exact_bits(parents: &[u32], dfs: &crate::dfs::DfsLayout, raw: &[bool]) -> Vec<u8> {
    let mut unknown_dfs = vec![0u8; parents.len().div_ceil(8)];
    for (record, &exact) in raw.iter().enumerate() {
        if !word_is_dir(parents[record]) && !exact {
            bitmap_set(&mut unknown_dfs, dfs.positions[record] as usize);
        }
    }
    let samples = crate::bitvec::build_superblocks(&unknown_dfs);
    let rank = crate::bitvec::RankSelect::new(&unknown_dfs, &samples);
    let mut result = vec![0u8; parents.len().div_ceil(8)];
    for record in 0..parents.len() {
        let exact = if word_is_dir(parents[record]) {
            let start = dfs.positions[record] as usize;
            let end = dfs.subtree_ends[record] as usize;
            rank.rank1(start) == rank.rank1(end)
        } else {
            raw[record]
        };
        if exact {
            bitmap_set(&mut result, record);
        }
    }
    result
}

// ── varint helpers (unsigned LEB128) ─────────────────────────────────────────

/// Bitwise-AND `src` into `dst` (same length), 8 bytes at a time. Native-endian
/// is correct here even though `to_ne_bytes`/`from_ne_bytes` normally deserve
/// suspicion: the AND is bitwise, both operands are trigram-index bytes in the
/// same in-memory representation, and the result is never persisted or sent
/// across a byte-order boundary — it only ever feeds the bit-scan below.
fn and_into(dst: &mut [u8], src: &[u8]) {
    let (dst_words, dst_tail) = dst.as_chunks_mut::<8>();
    let (src_words, src_tail) = src.as_chunks::<8>();
    for (d, s) in dst_words.iter_mut().zip(src_words) {
        *d = (u64::from_ne_bytes(*d) & u64::from_ne_bytes(*s)).to_ne_bytes();
    }
    for (d, &s) in dst_tail.iter_mut().zip(src_tail) {
        *d &= s;
    }
}

/// Bitwise-OR `src` into `dst` (same length). See [`and_into`] for the
/// native-endian rationale, which applies identically here.
fn or_into(dst: &mut [u8], src: &[u8]) {
    let (dst_words, dst_tail) = dst.as_chunks_mut::<8>();
    let (src_words, src_tail) = src.as_chunks::<8>();
    for (d, s) in dst_words.iter_mut().zip(src_words) {
        *d = (u64::from_ne_bytes(*d) | u64::from_ne_bytes(*s)).to_ne_bytes();
    }
    for (d, &s) in dst_tail.iter_mut().zip(src_tail) {
        *d |= s;
    }
}

impl ArchivedArena {
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    pub fn format_version(&self) -> u32 {
        self.format_version
    }

    /// Parent record index for entry `idx`. Returns `PARENT_NONE` for roots.
    #[inline]
    pub fn parent(&self, idx: u32) -> u32 {
        unpack_parent(self.parents[idx as usize])
    }

    /// Parent edge retained by the deterministic DFS spanning forest.
    ///
    /// The raw parent column can contain cycles. DFS necessarily skips one
    /// edge in every cycle, and a single subtree interval can describe only
    /// that resulting forest. Checking whether `idx` lies in its raw parent's
    /// subtree identifies the retained edges in O(1), making parent walks and
    /// interval-based path search agree without storing another column.
    #[inline]
    pub fn tree_parent(&self, idx: u32) -> u32 {
        canonical_tree_parent(
            idx,
            self.parent(idx),
            self.parents.len(),
            self.dfs_positions.as_slice(),
            self.dfs_ends.as_slice(),
        )
    }

    /// Whether entry `idx` is a directory.
    #[inline]
    pub fn is_dir(&self, idx: u32) -> bool {
        word_is_dir(self.parents[idx as usize])
    }

    /// Modification time for entry `idx`, seconds since 1970-01-01 UTC.
    #[inline]
    pub fn mtime(&self, idx: u32) -> u32 {
        self.mtimes[idx as usize]
    }

    /// File size for entry `idx`, in bytes (decoded from KiB column).
    /// Returns 0 when size is unknown (USN fallback path).
    #[inline]
    pub fn size_bytes(&self, idx: u32) -> u64 {
        self.sizes[idx as usize] as u64 * 1024
    }

    #[inline]
    pub fn size_exact(&self, idx: u32) -> bool {
        bitmap_get(self.size_exact_bits.as_slice(), idx as usize)
    }

    /// Depth-first position of record `idx` in tree order.
    #[inline]
    pub fn dfs_position(&self, idx: u32) -> u32 {
        self.dfs_positions[idx as usize]
    }

    /// Record index sitting at depth-first `position`.
    #[inline]
    pub fn dfs_record(&self, position: u32) -> u32 {
        self.dfs_records[position as usize]
    }

    /// Half-open span of depth-first positions covering `idx` and everything
    /// beneath it. Feed each position to [`Self::dfs_record`] to enumerate.
    ///
    /// A snapshot written before tree order existed has empty columns; the
    /// span is then empty rather than wrong, so callers must treat it as
    /// "no information" and fall back to an ancestor walk.
    #[inline]
    pub fn subtree(&self, idx: u32) -> std::ops::Range<u32> {
        if self.dfs_positions.is_empty() {
            return 0..0;
        }
        self.dfs_positions[idx as usize]..self.dfs_ends[idx as usize]
    }

    /// Number of records in `idx`'s subtree, itself included. O(1): it is the
    /// width of the interval, so nothing is visited.
    #[inline]
    pub fn subtree_len(&self, idx: u32) -> u32 {
        self.subtree(idx).len() as u32
    }

    /// Whether `descendant` lies beneath `ancestor` (or is it). O(1), versus
    /// the parent-chain walk it replaces.
    #[inline]
    pub fn is_descendant_of(&self, descendant: u32, ancestor: u32) -> bool {
        self.subtree(ancestor)
            .contains(&self.dfs_positions[descendant as usize])
    }

    /// Recursive size of `idx`'s subtree, itself included, in KiB. O(1): a
    /// subtraction of two entries of `dfs_size_prefix`, not a walk. A leaf's
    /// subtree is itself, so this also works — and agrees with `sizes[idx]`
    /// — for a plain file; callers do not need an `is_dir` branch.
    ///
    /// Unknown per-record sizes (the USN fallback path, see the `size` note
    /// in AGENTS.md) contribute zero, so a directory's total is a lower
    /// bound, not an exact figure, whenever any descendant's size is
    /// unknown. Saturates at `u32::MAX` KiB (~4 TiB) — the width `rank`'s
    /// sort key packs a size into — even though the underlying `u64` sum can
    /// exceed that on a very large subtree.
    ///
    /// A snapshot written before this column existed has an empty
    /// `dfs_size_prefix`; that can only happen if `dfs_positions` is also
    /// empty (both are populated together at build time), so the `subtree`
    /// empty-column fallback already guards this.
    #[inline]
    pub fn recursive_size_kib(&self, idx: u32) -> u32 {
        if self.dfs_size_prefix.is_empty() {
            return self.sizes[idx as usize];
        }
        let span = self.subtree(idx);
        let total =
            self.dfs_size_prefix[span.end as usize] - self.dfs_size_prefix[span.start as usize];
        total.min(u32::MAX as u64) as u32
    }

    /// Return blocks that can contain every trigram in `needle_lower`.
    pub fn candidate_blocks(&self, needle_lower: &[u8]) -> Option<Vec<u32>> {
        let mut blocks = Vec::new();
        let mut candidates = Vec::new();
        let mut hashes = Vec::new();
        self.candidate_blocks_into(needle_lower, &mut blocks, &mut candidates, &mut hashes)
            .then_some(blocks)
    }

    pub(crate) fn candidate_blocks_into(
        &self,
        needle_lower: &[u8],
        output: &mut Vec<u32>,
        candidates: &mut Vec<u8>,
        hashes: &mut Vec<usize>,
    ) -> bool {
        output.clear();
        candidates.clear();
        hashes.clear();
        if needle_lower.len() < 3 || self.trigram_index.is_empty() {
            return false;
        }

        let blocks = num_blocks(self.parents.len());
        let bytes = row_bytes(blocks);
        candidates.resize(bytes, u8::MAX);
        for_each_trigram(needle_lower, |hash| hashes.push(hash as usize));
        hashes.sort_unstable();
        hashes.dedup();
        for &hash in hashes.iter() {
            let row = &self.trigram_index.as_slice()[hash * bytes..(hash + 1) * bytes];
            and_into(candidates, row);
        }
        output.extend(
            (0..blocks)
                .filter(|&block| candidates[block / 8] & (1 << (block % 8)) != 0)
                .map(|block| block as u32),
        );
        true
    }

    /// Evaluate an AND-of-OR required-literal proof directly over trigram rows.
    pub(crate) fn candidate_blocks_for_clauses(
        &self,
        clauses: &[crate::literals::Clause],
    ) -> Option<Vec<u32>> {
        if clauses.is_empty() || self.trigram_index.is_empty() {
            return None;
        }
        let blocks = num_blocks(self.parents.len());
        let bytes = row_bytes(blocks);
        let mut candidates = vec![u8::MAX; bytes];
        let mut clause_bits = vec![0u8; bytes];
        let mut literal_bits = vec![0u8; bytes];
        let mut hashes = Vec::new();

        for clause in clauses {
            clause_bits.fill(0);
            for literal in clause {
                if literal.len() < 3 {
                    return None;
                }
                literal_bits.fill(u8::MAX);
                hashes.clear();
                for_each_trigram(literal, |hash| hashes.push(hash as usize));
                hashes.sort_unstable();
                hashes.dedup();
                for &hash in &hashes {
                    let row = &self.trigram_index.as_slice()[hash * bytes..(hash + 1) * bytes];
                    and_into(&mut literal_bits, row);
                }
                or_into(&mut clause_bits, &literal_bits);
            }
            and_into(&mut candidates, &clause_bits);
        }

        Some(
            (0..blocks)
                .filter(|&block| candidates[block / 8] & (1 << (block % 8)) != 0)
                .map(|block| block as u32)
                .collect(),
        )
    }

    /// Walk the parent chain to reconstruct the full path. Path segments are
    /// decoded from the name blob and joined with `sep`.
    ///
    /// Guarded against cycles and corrupt parent links: stops after 512 hops
    /// and returns whatever has been collected.
    pub fn full_path(&self, idx: u32, sep: char) -> String {
        let mut out = String::new();
        self.full_path_into(idx, sep, &mut out);
        out
    }

    /// Same as [`Self::full_path`], but appends into a caller-owned buffer
    /// instead of allocating a fresh `String`. `out` is cleared first, so a
    /// caller materializing many hits can reuse one buffer across calls.
    pub fn full_path_into(&self, mut idx: u32, sep: char, out: &mut String) {
        #[cfg(test)]
        FULL_PATH_CALLS.with(|calls| calls.set(calls.get() + 1));
        out.clear();
        // Record indices, not name bytes: the chain is walked once to find
        // the root, then a second time in reverse to decode names in path
        // order, so nothing but a fixed-size array of u32s is held across
        // the two passes.
        let mut chain = [0u32; 512];
        let mut len = 0usize;
        loop {
            chain[len] = idx;
            len += 1;
            // `tree_parent` additionally reads the hot DFS-position column
            // and one cold subtree end per hop so corrupt cycles use exactly
            // the same spanning forest as interval-based path search.
            let parent = self.tree_parent(idx);
            if parent == PARENT_NONE || len == chain.len() {
                break;
            }
            idx = parent;
        }
        let mut name_buf = Vec::new();
        let mut first = true;
        for &record in chain[..len].iter().rev() {
            self.name_into(record, &mut name_buf);
            if name_buf == b"." {
                continue;
            }
            if !first {
                out.push(sep);
            }
            first = false;
            out.push_str(&String::from_utf8_lossy(&name_buf));
        }
    }
}

#[cfg(test)]
thread_local! {
    static FULL_PATH_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_full_path_calls() {
    FULL_PATH_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn full_path_calls() -> usize {
    FULL_PATH_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests;
