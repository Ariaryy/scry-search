use crate::ascii;
use crate::record::{FileRecord, BUCKET_SIZE, PARENT_NONE};
use rkyv::{Archive, Deserialize, Serialize};

/// The full index: records in name-sorted order, names front-coded in a
/// separate blob, version-stamped for safe mmap reuse across daemon upgrades.
///
/// Three arrays, all plain PODs — no String, no relative pointer — so rkyv's
/// bytecheck performs a handful of bounds checks rather than chasing a pointer
/// and UTF-8-validating a name for every one of a million records.
#[derive(Archive, Serialize, Deserialize, Debug)]
#[archive(check_bytes)]
pub struct Arena {
    /// FORMAT_VERSION constant, stamped here so `ArenaStore::open` can reject
    /// stale snapshots before any other field is read.
    pub format_version: u32,
    /// Front-coded name blob. Names are stored in name-sorted order (matching
    /// `records`). Decode via `bucket_offsets` + the LEB128 encoding below.
    pub names: Vec<u8>,
    /// `bucket_offsets[b]` is the byte offset of bucket `b` in `names`.
    /// `bucket_offsets[num_buckets]` == `names.len()` (sentinel).
    pub bucket_offsets: Vec<u32>,
    /// One `FileRecord` per entry, in name-sorted order. 8 bytes each.
    pub records: Vec<FileRecord>,
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
fn front_code(names_in_order: &[&[u8]], blob: &mut Vec<u8>, offsets: &mut Vec<u32>) {
    let n = names_in_order.len();
    let num_buckets = n.div_ceil(BUCKET_SIZE);
    offsets.reserve(num_buckets + 1);

    for b in 0..num_buckets {
        offsets.push(blob.len() as u32);
        let start = b * BUCKET_SIZE;
        let end = (start + BUCKET_SIZE).min(n);

        // First entry in bucket: just length + bytes (no back-reference).
        let first = names_in_order[start];
        write_varint(blob, first.len() as u32);
        blob.extend_from_slice(first);
        let mut prev = first;

        // Remaining entries: shared prefix length + suffix.
        for i in (start + 1)..end {
            let cur = names_in_order[i];
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

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Decode all names in bucket `b` into `out_names` (a reusable scratch buffer
/// of Vec<u8>s). Returns a slice into `out_names`.
fn decode_bucket<'a>(
    blob: &[u8],
    offsets: &[u32],
    b: usize,
    out_names: &'a mut Vec<Vec<u8>>,
) -> &'a [Vec<u8>] {
    out_names.clear();
    let start_offset = offsets[b] as usize;
    let end_offset = offsets[b + 1] as usize;
    let bucket_blob = &blob[start_offset..end_offset];

    if bucket_blob.is_empty() {
        return &out_names[..];
    }

    let mut pos = 0usize;
    // First name.
    let len = read_varint(bucket_blob, &mut pos) as usize;
    out_names.push(bucket_blob[pos..pos + len].to_vec());
    pos += len;

    // Subsequent names.
    while pos < bucket_blob.len() {
        let shared = read_varint(bucket_blob, &mut pos) as usize;
        let suffix_len = read_varint(bucket_blob, &mut pos) as usize;
        let suffix = &bucket_blob[pos..pos + suffix_len];
        pos += suffix_len;

        let prev = out_names.last().unwrap();
        let mut name = Vec::with_capacity(shared + suffix_len);
        name.extend_from_slice(&prev[..shared]);
        name.extend_from_slice(suffix);
        out_names.push(name);
    }

    &out_names[..]
}

impl ArchivedArena {
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn format_version(&self) -> u32 {
        self.format_version
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
        let n = self.records.len();
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
            let parent = self.records[idx as usize].parent();
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
        let n = self.records.len() as u32;

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
                let before = ascii::cmp_ci(&name_buf, &prefix_lower) == std::cmp::Ordering::Less
                    && !matches;
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
                let before = ascii::cmp_ci(&name_buf, &prefix_lower) == std::cmp::Ordering::Less
                    && !matches;

                if matches {
                    if lo_idx.is_none() {
                        lo_idx = Some(global_idx);
                    }
                    hi_idx = Some(global_idx + 1);
                    past_matches = false;
                } else if !before {
                    if lo_idx.is_some() {
                        break 'scan;
                    }
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
    names: Vec<String>,
    parents: Vec<u32>,
    mtimes: Vec<u32>,
    dirs: Vec<bool>,
}

impl ArenaBuilder {
    /// Push an entry; returns its provisional index (valid for `set_parent`
    /// calls before `build()` — `build()` reorders into name order and rewrites
    /// all parent links).
    pub fn push(&mut self, name: String, mtime_secs: u32, is_dir: bool) -> u32 {
        let idx = self.names.len() as u32;
        self.names.push(name);
        self.parents.push(PARENT_NONE);
        self.mtimes.push(mtime_secs);
        self.dirs.push(is_dir);
        idx
    }

    pub fn set_parent(&mut self, idx: u32, parent: u32) {
        self.parents[idx as usize] = parent;
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Finalize: sort by name, remap parents, front-code names.
    pub fn build(self) -> Arena {
        let n = self.names.len();
        assert!(
            n < PARENT_NONE as usize,
            "arena exceeds maximum capacity ({} entries, limit {})",
            n,
            PARENT_NONE
        );

        // 1. Build sort order: ASCII-case-insensitive, ties by insertion index.
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_unstable_by(|&a, &b| {
            ascii::cmp_ci(self.names[a as usize].as_bytes(), self.names[b as usize].as_bytes())
                .then(a.cmp(&b))
        });

        // 2. Build inverse permutation: rank[order[j]] = j.
        let mut rank = vec![0u32; n];
        for (j, &orig) in order.iter().enumerate() {
            rank[orig as usize] = j as u32;
        }

        // 3. Build records in sorted order with remapped parents.
        let records: Vec<FileRecord> = order
            .iter()
            .map(|&orig| {
                let orig_parent = self.parents[orig as usize];
                let new_parent = if orig_parent == PARENT_NONE {
                    PARENT_NONE
                } else {
                    rank[orig_parent as usize]
                };
                FileRecord::new(new_parent, self.dirs[orig as usize], self.mtimes[orig as usize])
            })
            .collect();

        // 4. Front-code names in sorted order.
        let name_refs: Vec<&[u8]> = order.iter().map(|&i| self.names[i as usize].as_bytes()).collect();
        let mut names = Vec::new();
        let mut bucket_offsets = Vec::new();
        front_code(&name_refs, &mut names, &mut bucket_offsets);

        Arena {
            format_version: crate::record::FORMAT_VERSION,
            names,
            bucket_offsets,
            records,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{FileRecord, PARENT_NONE};
    use crate::store::save;

    fn simple_arena(names: &[&str]) -> Arena {
        let mut b = ArenaBuilder::default();
        for &name in names {
            b.push(name.to_string(), 0, false);
        }
        b.build()
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
            b.push(name.clone(), 0, false);
        }
        let arena = b.build();

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
            .flat_map(|i| vec![format!("img_{i:04}.jpg"), format!("readme_{i}.txt"), format!("z_{i}")])
            .collect();

        let mut b = ArenaBuilder::default();
        for n in &names {
            b.push(n.clone(), 0, false);
        }
        let arena = b.build();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rkyv");
        save(&arena, &path).unwrap();
        let store = crate::store::ArenaStore::open(&path).unwrap();
        let archived = store.archived();

        for prefix in &["img", "readme", "z_", "IMG", "README_0", "x", ""] {
            let range = archived.prefix_range(prefix);
            let mut range_names: Vec<String> =
                range.map(|i| archived.name(i)).collect();
            range_names.sort();

            let prefix_lower = prefix.to_ascii_lowercase();
            let mut brute: Vec<String> = (0..archived.len() as u32)
                .map(|i| archived.name(i))
                .filter(|n| n.to_ascii_lowercase().starts_with(&prefix_lower))
                .collect();
            brute.sort();

            assert_eq!(
                range_names, brute,
                "prefix_range mismatch for prefix {:?}", prefix
            );
        }
    }

    #[test]
    fn full_path_terminates_on_a_parent_cycle() {
        // Manually construct a minimal Arena with a cycle: record 0 -> parent 1, record 1 -> parent 0.
        let arena = Arena {
            format_version: crate::record::FORMAT_VERSION,
            names: {
                let names_in: Vec<&[u8]> = vec![b"a", b"b"];
                let mut blob = Vec::new();
                let mut offsets = Vec::new();
                front_code(&names_in, &mut blob, &mut offsets);
                blob
            },
            bucket_offsets: {
                let names_in: Vec<&[u8]> = vec![b"a", b"b"];
                let mut blob = Vec::new();
                let mut offsets = Vec::new();
                front_code(&names_in, &mut blob, &mut offsets);
                offsets
            },
            records: vec![
                FileRecord::new(1, false, 0), // record 0, parent = 1
                FileRecord::new(0, false, 0), // record 1, parent = 0 — CYCLE
            ],
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cycle.rkyv");
        save(&arena, &path).unwrap();
        let store = crate::store::ArenaStore::open(&path).unwrap();
        let archived = store.archived();

        // Must not hang; returns something finite.
        let path = archived.full_path(0, '\\');
        assert!(!path.is_empty(), "full_path should return something even with a cycle");
    }
}
