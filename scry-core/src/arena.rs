use crate::ascii;
use crate::frnmap::FrnEntry;
use crate::record::{bytes_to_size_kib, pack_parent, unpack_parent, word_is_dir, BUCKET_SIZE, PARENT_NONE};
use crate::trigram::{for_each_trigram, num_blocks, row_bytes, TRIGRAM_BLOCK, TRIGRAM_ROWS};
use rkyv::{Archive, Deserialize, Serialize};

/// The full index: entries in name-sorted order, names front-coded in a
/// separate blob, version-stamped for safe mmap reuse across daemon upgrades.
///
/// Five arrays, all plain PODs — no String, no relative pointer — so rkyv's
/// bytecheck performs a handful of bounds checks rather than chasing a pointer
/// and UTF-8-validating a name for every one of a million records.
///
/// The hot/cold split is deliberate and load-bearing:
/// - `parents` is touched by `full_path` on every displayed result. Packing it
///   alone means a 64-byte cache line holds 16 useful hops instead of 5 (as
///   it would interleaved with mtime and size).
/// - `mtimes` and `sizes` are read only during delta metadata lookup and
///   compaction, never on the query path. Kept in their own arrays so the OS
///   can evict those pages between compaction bursts.
/// These arrays are index-parallel: `parents[i]`, `mtimes[i]`, and `sizes[i]`
/// all describe the same entry. A future change that pushes to one without
/// the others silently corrupts the index.
#[derive(Archive, Serialize, Deserialize, Debug)]
#[archive(check_bytes)]
pub struct Arena {
    /// FORMAT_VERSION constant, stamped here so `ArenaStore::open` can reject
    /// stale snapshots before any other field is read.
    pub format_version: u32,
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
    /// Cold: same eviction rationale as `mtimes`. 0 means unknown (USN path),
    /// not empty — see the `size` limitation note in CLAUDE.md.
    pub sizes: Vec<u32>,
    /// Trigram row matrix; each row has one LSB-first bit per 1024 records.
    pub trigram_index: Vec<u8>,
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
}

// ── varint helpers (unsigned LEB128) ─────────────────────────────────────────

fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        } else {
            out.push(byte | 0x80);
        }
    }
}

fn read_varint(buf: &[u8], pos: &mut usize) -> u32 {
    let mut result = 0u32;
    let mut shift = 0u32;
    loop {
        let byte = buf[*pos];
        *pos += 1;
        result |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    result
}

// ── front-coding encode / decode ─────────────────────────────────────────────

/// Encode `names_in_order` (already in sorted order) into `blob` and
/// `offsets`. `offsets` will have `ceil(n/BUCKET_SIZE) + 1` entries,
/// with `offsets[last] == blob.len()`.
fn front_code(
    names_in_order: &[&[u8]],
    blob: &mut Vec<u8>,
    offsets: &mut Vec<u32>,
    trigram_index: &mut [u8],
    row_len: usize,
) {
    let n = names_in_order.len();
    let num_buckets = n.div_ceil(BUCKET_SIZE);
    offsets.reserve(num_buckets + 1);

    for b in 0..num_buckets {
        offsets.push(blob.len() as u32);
        let start = b * BUCKET_SIZE;
        let end = (start + BUCKET_SIZE).min(n);

        // First entry in bucket: just length + bytes (no back-reference).
        let first = names_in_order[start];
        index_trigrams(start, first, trigram_index, row_len);
        write_varint(blob, first.len() as u32);
        blob.extend_from_slice(first);
        let mut prev = first;

        // Remaining entries: shared prefix length + suffix.
        for (index, cur) in names_in_order[(start + 1)..end].iter().enumerate() {
            index_trigrams(start + 1 + index, cur, trigram_index, row_len);
            let shared = common_prefix_len(prev, cur);
            write_varint(blob, shared as u32);
            let suffix = &cur[shared..];
            write_varint(blob, suffix.len() as u32);
            blob.extend_from_slice(suffix);
            prev = cur;
        }
    }
    offsets.push(blob.len() as u32); // sentinel
}

fn index_trigrams(index: usize, name: &[u8], matrix: &mut [u8], row_len: usize) {
    if matrix.is_empty() {
        return;
    }
    let block = index / TRIGRAM_BLOCK;
    for_each_trigram(name, |hash| {
        matrix[hash as usize * row_len + block / 8] |= 1 << (block % 8);
    });
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
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

    /// Decode the name of record `idx` into `out`, clearing it first.
    /// Reuse one buffer across a scan to avoid per-record allocation.
    pub fn name_into(&self, idx: u32, out: &mut Vec<u8>) {
        out.clear();
        let b = (idx as usize) / BUCKET_SIZE;
        let pos_in_bucket = (idx as usize) % BUCKET_SIZE;

        let blob: &[u8] = &self.names;
        let offsets: &[u32] = &self.bucket_offsets;
        let start_offset = offsets[b] as usize;
        let end_offset = offsets[b + 1] as usize;
        let bucket_blob = &blob[start_offset..end_offset];

        let mut p = 0usize;
        // First name in bucket.
        let first_len = read_varint(bucket_blob, &mut p) as usize;
        out.extend_from_slice(&bucket_blob[p..p + first_len]);
        p += first_len;

        if pos_in_bucket == 0 {
            return;
        }

        // Decode subsequent names, updating `out` in place.
        for _ in 0..pos_in_bucket {
            let shared = read_varint(bucket_blob, &mut p) as usize;
            let suffix_len = read_varint(bucket_blob, &mut p) as usize;
            let suffix_start = p;
            p += suffix_len;
            // Truncate to shared, then append suffix.
            out.truncate(shared);
            out.extend_from_slice(&bucket_blob[suffix_start..suffix_start + suffix_len]);
        }
    }

    /// Convenience wrapper. Allocates; do not call in a loop.
    pub fn name(&self, idx: u32) -> String {
        let mut buf = Vec::new();
        self.name_into(idx, &mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Walk every record in name order, calling `f(idx, name_bytes)` for each.
    /// Return `ControlFlow::Break` from `f` to stop early.
    /// Decodes each bucket sequentially — cache-friendly, allocation-free (one
    /// scratch `Vec<u8>` reused across the whole scan).
    pub fn for_each_name(&self, mut f: impl FnMut(u32, &[u8]) -> std::ops::ControlFlow<()>) {
        let n = self.parents.len();
        let blob: &[u8] = &self.names;
        let offsets: &[u32] = &self.bucket_offsets;
        let num_buckets = offsets.len().saturating_sub(1);

        let mut name_buf = Vec::new();

        'outer: for b in 0..num_buckets {
            let start = b * BUCKET_SIZE;
            let end = (start + BUCKET_SIZE).min(n);
            let start_offset = offsets[b] as usize;
            let end_offset = offsets[b + 1] as usize;
            let bucket_blob = &blob[start_offset..end_offset];

            if bucket_blob.is_empty() {
                continue;
            }

            let mut p = 0usize;
            // First name.
            let first_len = read_varint(bucket_blob, &mut p) as usize;
            name_buf.clear();
            name_buf.extend_from_slice(&bucket_blob[p..p + first_len]);
            p += first_len;

            if f(start as u32, &name_buf).is_break() {
                break 'outer;
            }

            for i in (start + 1)..end {
                let shared = read_varint(bucket_blob, &mut p) as usize;
                let suffix_len = read_varint(bucket_blob, &mut p) as usize;
                let suffix_start = p;
                p += suffix_len;
                name_buf.truncate(shared);
                name_buf.extend_from_slice(&bucket_blob[suffix_start..suffix_start + suffix_len]);

                if f(i as u32, &name_buf).is_break() {
                    break 'outer;
                }
            }
        }
    }

    /// Walk names whose record indices fall within `range`.
    pub fn for_each_name_in(
        &self,
        range: std::ops::Range<u32>,
        mut f: impl FnMut(u32, &[u8]) -> std::ops::ControlFlow<()>,
    ) {
        let end = range.end.min(self.parents.len() as u32);
        if range.start >= end {
            return;
        }
        let first_bucket = range.start as usize / BUCKET_SIZE;
        let last_bucket = (end as usize - 1) / BUCKET_SIZE;
        let mut name = Vec::new();
        'outer: for bucket in first_bucket..=last_bucket {
            let bucket_start = bucket * BUCKET_SIZE;
            let bucket_end = (bucket_start + BUCKET_SIZE).min(self.parents.len());
            let blob_start = self.bucket_offsets[bucket] as usize;
            let blob_end = self.bucket_offsets[bucket + 1] as usize;
            let blob = &self.names.as_slice()[blob_start..blob_end];
            let mut pos = 0;
            let first_len = read_varint(blob, &mut pos) as usize;
            name.clear();
            name.extend_from_slice(&blob[pos..pos + first_len]);
            pos += first_len;
            for idx in bucket_start..bucket_end {
                if idx != bucket_start {
                    let shared = read_varint(blob, &mut pos) as usize;
                    let suffix_len = read_varint(blob, &mut pos) as usize;
                    name.truncate(shared);
                    name.extend_from_slice(&blob[pos..pos + suffix_len]);
                    pos += suffix_len;
                }
                if idx >= range.start as usize
                    && idx < end as usize
                    && f(idx as u32, &name).is_break()
                {
                    break 'outer;
                }
            }
        }
    }

    /// Return blocks that can contain every trigram in `needle_lower`.
    pub fn candidate_blocks(&self, needle_lower: &[u8]) -> Option<Vec<u32>> {
        if needle_lower.len() < 3 || self.trigram_index.is_empty() {
            return None;
        }
        let blocks = num_blocks(self.parents.len());
        let bytes = row_bytes(blocks);
        let mut hashes = Vec::new();
        for_each_trigram(needle_lower, |hash| hashes.push(hash as usize));
        hashes.sort_unstable();
        hashes.dedup();
        let mut candidates = vec![u8::MAX; bytes];
        for hash in hashes {
            let row = &self.trigram_index.as_slice()[hash * bytes..(hash + 1) * bytes];
            for (candidate, &value) in candidates.iter_mut().zip(row) {
                *candidate &= value;
            }
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
    pub fn full_path(&self, mut idx: u32, sep: char) -> String {
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let mut name_buf = Vec::new();
        for _ in 0..512 {
            self.name_into(idx, &mut name_buf);
            parts.push(name_buf.clone());
            // `self.parent(idx)` reads only from the hot `parents` column;
            // mtime and size stay evicted.
            let parent = self.parent(idx);
            if parent == PARENT_NONE {
                break;
            }
            idx = parent;
        }
        parts.reverse();
        let sep_str = sep.to_string();
        parts
            .iter()
            .map(|b| String::from_utf8_lossy(b))
            .collect::<Vec<_>>()
            .join(&sep_str)
    }

    /// Range of record indices (contiguous, in name order) whose names begin
    /// with `prefix` (ASCII-case-insensitive).
    pub fn prefix_range(&self, prefix: &str) -> std::ops::Range<u32> {
        let prefix_lower: Vec<u8> = prefix.bytes().map(|b| b.to_ascii_lowercase()).collect();
        let n = self.parents.len() as u32;

        if prefix_lower.is_empty() {
            return 0..n;
        }

        let num_buckets = self.bucket_offsets.len().saturating_sub(1);

        // Binary search over bucket heads to find the first bucket that might
        // contain a match.
        let find_bucket_lo = |target: &[u8]| -> usize {
            // Find the last bucket whose first name < target.
            let mut lo = 0usize;
            let mut hi = num_buckets;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let head = self.bucket_head(mid);
                if ascii::cmp_ci(&head, target) == std::cmp::Ordering::Less {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo.saturating_sub(1)
        };

        let start_bucket = find_bucket_lo(&prefix_lower);

        // Scan forward from start_bucket to find precise lo and hi record indices.
        let mut lo_idx: Option<u32> = None;
        let mut hi_idx: Option<u32> = None;
        let mut past_matches = false;

        let mut name_buf = Vec::new();
        'scan: for b in start_bucket..num_buckets {
            let bstart = (b * BUCKET_SIZE) as u32;
            let bend = bstart + (BUCKET_SIZE as u32).min(n - bstart);

            // Decode this bucket's first name to check if we've gone past all matches.
            let head = self.bucket_head(b);
            // If the head is already greater than prefix with trailing 0xff, we're done.
            if !past_matches {
                // Check if this bucket can possibly contain matches.
                let head_lower: Vec<u8> = head.iter().map(|&b| b.to_ascii_lowercase()).collect();
                if head_lower.len() >= prefix_lower.len()
                    && ascii::cmp_ci(&head_lower[..prefix_lower.len()], &prefix_lower)
                        == std::cmp::Ordering::Greater
                    && lo_idx.is_some()
                {
                    break 'scan;
                }
            }

            // Decode all names in this bucket and check each.
            let start_offset = self.bucket_offsets[b] as usize;
            let end_offset = self.bucket_offsets[b + 1] as usize;
            let bucket_blob = &self.names.as_slice()[start_offset..end_offset];

            if bucket_blob.is_empty() {
                continue;
            }

            let mut p = 0usize;
            let first_len = read_varint(bucket_blob, &mut p) as usize;
            name_buf.clear();
            name_buf.extend_from_slice(&bucket_blob[p..p + first_len]);
            p += first_len;

            // Check first name
            {
                let matches = ascii::starts_with_ci(&name_buf, &prefix_lower);
                let before =
                    ascii::cmp_ci(&name_buf, &prefix_lower) == std::cmp::Ordering::Less && !matches;
                if matches {
                    if lo_idx.is_none() {
                        lo_idx = Some(bstart);
                    }
                    hi_idx = Some(bstart + 1);
                } else if past_matches && !before {
                    break 'scan;
                }
                if !matches && !before && lo_idx.is_some() {
                    past_matches = true;
                }
            }

            for rel in 1..(bend - bstart) {
                let shared = read_varint(bucket_blob, &mut p) as usize;
                let suffix_len = read_varint(bucket_blob, &mut p) as usize;
                let suffix_start = p;
                p += suffix_len;
                name_buf.truncate(shared);
                name_buf.extend_from_slice(&bucket_blob[suffix_start..suffix_start + suffix_len]);

                let global_idx = bstart + rel;
                let matches = ascii::starts_with_ci(&name_buf, &prefix_lower);
                let before =
                    ascii::cmp_ci(&name_buf, &prefix_lower) == std::cmp::Ordering::Less && !matches;

                if matches {
                    if lo_idx.is_none() {
                        lo_idx = Some(global_idx);
                    }
                    hi_idx = Some(global_idx + 1);
                    past_matches = false;
                } else if !before && lo_idx.is_some() {
                    break 'scan;
                }
            }
        }

        match (lo_idx, hi_idx) {
            (Some(lo), Some(hi)) => lo..hi,
            _ => 0..0,
        }
    }

    /// Decode just the first name in bucket `b` (cheap — no back-reference needed).
    fn bucket_head(&self, b: usize) -> Vec<u8> {
        let start_offset = self.bucket_offsets[b] as usize;
        let end_offset = self.bucket_offsets[b + 1] as usize;
        let bucket_blob = &self.names.as_slice()[start_offset..end_offset];
        if bucket_blob.is_empty() {
            return Vec::new();
        }
        let mut p = 0usize;
        let len = read_varint(bucket_blob, &mut p) as usize;
        bucket_blob[p..p + len].to_vec()
    }
}

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
    frns: Vec<Option<u64>>,
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
            frns: Vec::with_capacity(entries),
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
        let idx = self.name_ends.len() as u32;
        self.staging_names.extend_from_slice(name);
        self.name_ends.push(self.staging_names.len() as u32);
        self.parents.push(PARENT_NONE);
        self.mtimes.push(mtime_secs);
        self.dirs.push(is_dir);
        self.frns.push(frn);
        self.sizes.push(size_bytes);
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

        (
            Arena {
                format_version: crate::record::FORMAT_VERSION,
                names,
                bucket_offsets,
                parents: out_parents,
                mtimes: out_mtimes,
                sizes: out_sizes,
                trigram_index,
            },
            frn_entries,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::PARENT_NONE;
    use crate::store::save;

    fn simple_arena(names: &[&str]) -> Arena {
        let mut b = ArenaBuilder::default();
        for &name in names {
            b.push(name, 0, false);
        }
        b.build().0
    }

    #[test]
    fn varint_round_trips() {
        for v in [0u32, 1, 127, 128, 300, 70000, u32::MAX] {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            let mut pos = 0;
            assert_eq!(read_varint(&buf, &mut pos), v);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn front_coding_round_trips_every_name() {
        // 200 names: IMG series, z series, short, long
        let mut raw: Vec<String> = (0..100).map(|i| format!("IMG_{i:04}.JPG")).collect();
        raw.extend((0..80).map(|i| format!("z{i}")));
        raw.push("a".to_string());
        raw.push("".to_string());
        raw.push("b".to_string());
        raw.extend((0..17).map(|i| format!("node_modules_{i:03}")));
        assert!(raw.len() >= 200); // sanity

        let mut b = ArenaBuilder::default();
        for name in &raw {
            b.push(name, 0, false);
        }
        let arena = b.build().0;

        // Reconstruct what the sorted order should be.
        let mut expected: Vec<String> = raw.clone();
        expected.sort_by(|a, b| {
            a.to_ascii_lowercase()
                .cmp(&b.to_ascii_lowercase())
                .then(a.cmp(b))
        });
        // deduplicate sort stably for tie-breaking by index (arena does it by original index)
        // Actually just check by name content not by index since we only care names decode correctly.

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rkyv");
        save(&arena, &path).unwrap();
        let store = crate::store::ArenaStore::open(&path).unwrap();
        let archived = store.archived();

        assert_eq!(archived.len(), raw.len());
        for i in 0..archived.len() {
            let decoded = archived.name(i as u32);
            // Must be a valid name from our input set.
            assert!(
                raw.iter().any(|n| n == &decoded),
                "decoded name {decoded:?} not in input set (index {i})"
            );
        }

        // Also verify all names are present (no duplicates lost).
        let mut all_decoded: Vec<String> = (0..archived.len() as u32)
            .map(|i| archived.name(i))
            .collect();
        let mut all_raw = raw.clone();
        all_decoded.sort();
        all_raw.sort();
        assert_eq!(all_decoded, all_raw);
    }

    #[test]
    fn bucket_offsets_are_monotonic_with_sentinel() {
        let arena = simple_arena(&["alpha", "beta", "gamma"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rkyv");
        save(&arena, &path).unwrap();
        let store = crate::store::ArenaStore::open(&path).unwrap();
        let archived = store.archived();

        let offsets: Vec<u32> = archived.bucket_offsets.iter().copied().collect();
        for w in offsets.windows(2) {
            assert!(w[0] <= w[1], "offsets not monotonic: {:?}", offsets);
        }
        assert_eq!(*offsets.last().unwrap() as usize, archived.names.len());
    }

    #[test]
    fn prefix_range_matches_a_brute_force_scan() {
        let names: Vec<String> = (0..100)
            .flat_map(|i| {
                vec![
                    format!("img_{i:04}.jpg"),
                    format!("IMG archive {i:04}.png"),
                    format!("readme_{i}.txt"),
                    format!("readme-{i:03}-final.md"),
                    format!("alpha.beta.{i:03}"),
                    format!("alphabet_{i:03}"),
                    format!("z_{i}"),
                    format!("éclair_{i:03}"),
                ]
            })
            .collect();

        let mut b = ArenaBuilder::default();
        for n in &names {
            b.push(n, 0, false);
        }
        let arena = b.build().0;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rkyv");
        save(&arena, &path).unwrap();
        let store = crate::store::ArenaStore::open(&path).unwrap();
        let archived = store.archived();

        for prefix in &[
            "img",
            "IMG ",
            "readme",
            "README_0",
            "alpha.",
            "alphabet_09",
            "z_",
            "éclair_",
            "x",
            "",
        ] {
            let range = archived.prefix_range(prefix);
            let mut range_names: Vec<String> = range.map(|i| archived.name(i)).collect();
            range_names.sort();

            let prefix_lower = prefix.to_ascii_lowercase();
            let mut brute: Vec<String> = (0..archived.len() as u32)
                .map(|i| archived.name(i))
                .filter(|n| n.to_ascii_lowercase().starts_with(&prefix_lower))
                .collect();
            brute.sort();

            assert_eq!(
                range_names, brute,
                "prefix_range mismatch for prefix {:?}",
                prefix
            );
        }
    }

    #[test]
    fn full_path_terminates_on_a_parent_cycle() {
        // Manually construct a minimal Arena with a cycle: record 0 -> parent 1, record 1 -> parent 0.
        let (names, bucket_offsets) = {
            let names_in: Vec<&[u8]> = vec![b"a", b"b"];
            let mut blob = Vec::new();
            let mut offsets = Vec::new();
            front_code(&names_in, &mut blob, &mut offsets, &mut [], 0);
            (blob, offsets)
        };
        let arena = Arena {
            format_version: crate::record::FORMAT_VERSION,
            names,
            bucket_offsets,
            parents: vec![
                pack_parent(1, false), // record 0, parent = 1
                pack_parent(0, false), // record 1, parent = 0 — CYCLE
            ],
            mtimes: vec![0, 0],
            sizes: vec![0, 0],
            trigram_index: Vec::new(),
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cycle.rkyv");
        save(&arena, &path).unwrap();
        let store = crate::store::ArenaStore::open(&path).unwrap();
        let archived = store.archived();

        // Must not hang; returns something finite.
        let path = archived.full_path(0, '\\');
        assert!(
            !path.is_empty(),
            "full_path should return something even with a cycle"
        );
    }

    /// The key equivalence test for the blob-backed staging refactor: building
    /// the same tree via `push` (str) and via `push_bytes` (raw bytes) must
    /// produce byte-identical serialized output. That single assertion covers
    /// sort order, parent remapping, and front-coding all at once.
    #[test]
    fn push_bytes_and_push_produce_identical_arenas() {
        let names: Vec<String> = (0..500)
            .map(|i| format!("file_{i:04}_{}.dat", i % 7))
            .collect();

        let mut b1 = ArenaBuilder::default();
        let root1 = b1.push("C:", 0, true);
        for name in &names {
            let idx = b1.push(name, 0, false);
            b1.set_parent(idx, root1);
        }
        let arena1 = b1.build().0;

        let mut b2 = ArenaBuilder::with_capacity(names.len() + 1, 0);
        let root2 = b2.push_bytes(b"C:", 0, true);
        for name in &names {
            let idx = b2.push_bytes(name.as_bytes(), 0, false);
            b2.set_parent(idx, root2);
        }
        let arena2 = b2.build().0;

        let bytes1 = rkyv::to_bytes::<_, 1024>(&arena1).unwrap();
        let bytes2 = rkyv::to_bytes::<_, 1024>(&arena2).unwrap();
        assert_eq!(
            bytes1.as_slice(),
            bytes2.as_slice(),
            "push and push_bytes must produce byte-identical arenas"
        );
    }

    #[test]
    fn builder_with_blob_staging_matches_string_staging() {
        // "String staging" here means the pre-refactor semantics: names
        // pushed one at a time via `push`, compared against a brute-force
        // sort/front-code done independently over owned `String`s. If the
        // blob-backed builder drifted from that behaviour, this test would
        // fail on sort order or front-coding content.
        let names = [
            "readme.txt",
            "Readme.txt",
            "a",
            "abc",
            "abcd",
            "z_last",
            "IMG_0001.jpg",
            "img_0002.jpg",
            "",
            "mid",
        ];

        let arena = simple_arena(&names);

        let mut expected: Vec<&str> = names.to_vec();
        expected.sort_by(|a, b| ascii::cmp_ci(a.as_bytes(), b.as_bytes()));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob_staging.rkyv");
        save(&arena, &path).unwrap();
        let store = crate::store::ArenaStore::open(&path).unwrap();
        let archived = store.archived();

        let actual: Vec<String> = (0..archived.len() as u32)
            .map(|i| archived.name(i))
            .collect();
        assert_eq!(actual, expected);
    }

    fn large_arena(count: usize) -> Arena {
        let mut builder = ArenaBuilder::default();
        for i in 0..count {
            builder.push(&format!("family_{:02}_file_{i:05}.dat", i % 37), 0, false);
        }
        builder.build().0
    }

    #[test]
    fn trigram_index_size_matches_formula() {
        let arena = large_arena(5_000);
        assert_eq!(
            arena.trigram_index.len(),
            TRIGRAM_ROWS * row_bytes(num_blocks(arena.len()))
        );
    }

    #[test]
    fn small_arena_has_empty_trigram_index() {
        assert!(large_arena(100).trigram_index.is_empty());
    }

    #[test]
    fn trigram_index_has_no_false_negatives() {
        let arena = large_arena(5_000);
        let bytes = row_bytes(num_blocks(arena.len()));
        for sample in 0..200 {
            let source_idx = sample * 23 % arena.len();
            let source = name_from_owned_arena(&arena, source_idx as u32);
            let length = 3 + sample % 6;
            let start = sample % (source.len() - length + 1);
            let needle = &source[start..start + length];
            for idx in 0..arena.len() {
                let name = name_from_owned_arena(&arena, idx as u32);
                if crate::ascii::contains_ci(&name, needle) {
                    for_each_trigram(needle, |hash| {
                        let block = idx / TRIGRAM_BLOCK;
                        assert_ne!(
                            arena.trigram_index[hash as usize * bytes + block / 8]
                                & (1 << (block % 8)),
                            0,
                            "missing block {block} for needle {:?}",
                            String::from_utf8_lossy(needle)
                        );
                    });
                }
            }
        }
    }

    fn name_from_owned_arena(arena: &Arena, idx: u32) -> Vec<u8> {
        let bucket = idx as usize / BUCKET_SIZE;
        let position = idx as usize % BUCKET_SIZE;
        let blob = &arena.names
            [arena.bucket_offsets[bucket] as usize..arena.bucket_offsets[bucket + 1] as usize];
        let mut cursor = 0;
        let len = read_varint(blob, &mut cursor) as usize;
        let mut name = blob[cursor..cursor + len].to_vec();
        cursor += len;
        for _ in 0..position {
            let shared = read_varint(blob, &mut cursor) as usize;
            let suffix = read_varint(blob, &mut cursor) as usize;
            name.truncate(shared);
            name.extend_from_slice(&blob[cursor..cursor + suffix]);
            cursor += suffix;
        }
        name
    }

    #[test]
    fn candidate_blocks_and_ranged_scan_are_sound() {
        let arena = large_arena(5_000);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigram.rkyv");
        save(&arena, &path).unwrap();
        let store = crate::store::ArenaStore::open(&path).unwrap();
        let archived = store.archived();

        for needle in [b"fam".as_slice(), b"file_01", b".dat"] {
            let candidates = archived.candidate_blocks(needle).unwrap();
            archived.for_each_name(|idx, name| {
                if crate::ascii::contains_ci(name, needle) {
                    assert!(candidates.contains(&(idx / TRIGRAM_BLOCK as u32)));
                }
                std::ops::ControlFlow::Continue(())
            });
        }
        assert!(archived.candidate_blocks(b"").is_none());
        assert!(archived.candidate_blocks(b"a").is_none());
        assert!(archived.candidate_blocks(b"ab").is_none());

        let mut full = Vec::new();
        archived.for_each_name(|idx, name| {
            full.push((idx, name.to_vec()));
            std::ops::ControlFlow::Continue(())
        });
        let mut ranged = Vec::new();
        archived.for_each_name_in(0..archived.len() as u32, |idx, name| {
            ranged.push((idx, name.to_vec()));
            std::ops::ControlFlow::Continue(())
        });
        assert_eq!(ranged, full);

        let mut mid = Vec::new();
        archived.for_each_name_in(5..45, |idx, name| {
            mid.push((idx, name.to_vec()));
            std::ops::ControlFlow::Continue(())
        });
        assert_eq!(mid, full[5..45]);
    }

    #[test]
    fn frn_map_points_at_correct_records() {
        let mut builder = ArenaBuilder::default();
        let mut expected = Vec::new();
        for i in 0..1_000u64 {
            let name = format!("name_{:04}", 999 - i);
            builder.push_bytes_with_frn(name.as_bytes(), 0, false, 10_000 + i * 17);
            expected.push((10_000 + i * 17, name));
        }
        let (arena, mut entries) = builder.build();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapped.rkyv");
        crate::store::save_with_sidecar(&arena, &mut entries, &path, |_| {}, |_| {}).unwrap();
        let store = crate::store::ArenaStore::open(&path).unwrap();
        let map = store.frn_map.as_ref().unwrap();
        for (frn, name) in expected {
            assert_eq!(store.archived().name(map.lookup(frn).unwrap()), name);
        }
    }
}
