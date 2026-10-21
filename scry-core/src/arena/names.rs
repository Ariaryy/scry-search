//! Front-coded name encoding, decoding, sorted lookup, and bucket walkers.

use crate::ascii;
use crate::record::BUCKET_SIZE;
use crate::trigram::{for_each_trigram, TRIGRAM_BLOCK};

pub(super) fn write_varint(out: &mut Vec<u8>, mut v: u32) {
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

pub(super) fn read_varint(buf: &[u8], pos: &mut usize) -> u32 {
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

/// Incremental owned name encoder. Bucket boundaries and shared-prefix
/// encoding depend only on push order. The owned arena builder feeds it
/// names in sorted order through `front_code`.
struct FrontCoder {
    blob: Vec<u8>,
    offsets: Vec<u32>,
    prev: Vec<u8>,
    pos_in_bucket: usize,
    absolute_index: usize,
}

impl FrontCoder {
    fn new(capacity_entries: usize, capacity_bytes: usize) -> Self {
        FrontCoder {
            blob: Vec::with_capacity(capacity_bytes),
            offsets: Vec::with_capacity(capacity_entries.div_ceil(BUCKET_SIZE) + 1),
            prev: Vec::new(),
            pos_in_bucket: 0,
            absolute_index: 0,
        }
    }

    fn push(&mut self, name: &[u8], trigram_index: &mut [u8], row_len: usize) {
        index_trigrams(self.absolute_index, name, trigram_index, row_len);
        if self.pos_in_bucket == 0 {
            self.offsets.push(self.blob.len() as u32);
            write_varint(&mut self.blob, name.len() as u32);
            self.blob.extend_from_slice(name);
        } else {
            let shared = common_prefix_len(&self.prev, name);
            write_varint(&mut self.blob, shared as u32);
            let suffix = &name[shared..];
            write_varint(&mut self.blob, suffix.len() as u32);
            self.blob.extend_from_slice(suffix);
        }
        self.prev.clear();
        self.prev.extend_from_slice(name);
        self.absolute_index += 1;
        self.pos_in_bucket += 1;
        if self.pos_in_bucket == BUCKET_SIZE {
            self.pos_in_bucket = 0;
        }
    }

    fn finish(mut self) -> (Vec<u8>, Vec<u32>) {
        self.offsets.push(self.blob.len() as u32); // sentinel
        (self.blob, self.offsets)
    }
}

/// Encode `names_in_order` (already in sorted order) into `blob` and
/// `offsets`. `offsets` will have `ceil(n/BUCKET_SIZE) + 1` entries,
/// with `offsets[last] == blob.len()`.
pub(super) fn front_code(
    names_in_order: &[&[u8]],
    blob: &mut Vec<u8>,
    offsets: &mut Vec<u32>,
    trigram_index: &mut [u8],
    row_len: usize,
) {
    let mut coder = FrontCoder::new(names_in_order.len(), 0);
    for &name in names_in_order {
        coder.push(name, trigram_index, row_len);
    }
    let (coded_blob, coded_offsets) = coder.finish();
    *blob = coded_blob;
    *offsets = coded_offsets;
}

pub(super) fn index_trigrams(index: usize, name: &[u8], matrix: &mut [u8], row_len: usize) {
    if matrix.is_empty() {
        return;
    }
    let block = index / TRIGRAM_BLOCK;
    for_each_trigram(name, |hash| {
        matrix[hash as usize * row_len + block / 8] |= 1 << (block % 8);
    });
}

pub(super) fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

impl super::ArchivedArena {
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
        self.for_each_name_with_repeats(|idx, name, _| f(idx, name));
    }

    /// The repeat flag is true only for a byte-identical name after an emitted
    /// predecessor in the same bucket. Bucket heads always start a new run.
    pub(crate) fn for_each_name_with_repeats(
        &self,
        mut f: impl FnMut(u32, &[u8], bool) -> std::ops::ControlFlow<()>,
    ) {
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

            if f(start as u32, &name_buf, false).is_break() {
                break 'outer;
            }

            for i in (start + 1)..end {
                let shared = read_varint(bucket_blob, &mut p) as usize;
                let suffix_len = read_varint(bucket_blob, &mut p) as usize;
                let suffix_start = p;
                p += suffix_len;
                let repeated = shared == name_buf.len() && suffix_len == 0;
                name_buf.truncate(shared);
                name_buf.extend_from_slice(&bucket_blob[suffix_start..suffix_start + suffix_len]);

                if f(i as u32, &name_buf, repeated).is_break() {
                    break 'outer;
                }
            }
        }
    }

    /// Walk names whose record indices fall within `range`.
    pub fn for_each_name_in(
        &self,
        range: std::ops::Range<u32>,
        f: impl FnMut(u32, &[u8]) -> std::ops::ControlFlow<()>,
    ) {
        self.for_each_name_in_buf(range, &mut Vec::new(), f);
    }

    /// As [`Self::for_each_name_in`], decoding into a caller-owned buffer so
    /// repeated small-range walks do not allocate.
    pub fn for_each_name_in_buf(
        &self,
        range: std::ops::Range<u32>,
        name: &mut Vec<u8>,
        mut f: impl FnMut(u32, &[u8]) -> std::ops::ControlFlow<()>,
    ) {
        self.for_each_name_in_buf_with_repeats(range, name, |idx, name, _| f(idx, name));
    }

    pub(crate) fn for_each_name_in_with_repeats(
        &self,
        range: std::ops::Range<u32>,
        f: impl FnMut(u32, &[u8], bool) -> std::ops::ControlFlow<()>,
    ) {
        self.for_each_name_in_buf_with_repeats(range, &mut Vec::new(), f);
    }

    /// As the ranged walker, with exact-repeat information. The first emitted
    /// record and every bucket head reset reuse, including mid-bucket ranges.
    pub(crate) fn for_each_name_in_buf_with_repeats(
        &self,
        range: std::ops::Range<u32>,
        name: &mut Vec<u8>,
        mut f: impl FnMut(u32, &[u8], bool) -> std::ops::ControlFlow<()>,
    ) {
        let end = range.end.min(self.parents.len() as u32);
        if range.start >= end {
            return;
        }
        let first_bucket = range.start as usize / BUCKET_SIZE;
        let last_bucket = (end as usize - 1) / BUCKET_SIZE;
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
                let mut repeated = false;
                if idx != bucket_start {
                    let shared = read_varint(blob, &mut pos) as usize;
                    let suffix_len = read_varint(blob, &mut pos) as usize;
                    repeated = shared == name.len() && suffix_len == 0;
                    name.truncate(shared);
                    name.extend_from_slice(&blob[pos..pos + suffix_len]);
                    pos += suffix_len;
                }
                if idx >= range.start as usize
                    && idx < end as usize
                    && f(idx as u32, name, repeated && idx > range.start as usize).is_break()
                {
                    break 'outer;
                }
            }
        }
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

    /// Smallest record index `i` such that `name(i) >= target` (ASCII
    /// case-insensitive), or `self.len()` if every name is smaller. Records
    /// are name-sorted, so this is a lower-bound binary search over bucket
    /// heads (mirroring `prefix_range`'s `find_bucket_lo`), followed by a
    /// linear decode within the boundary bucket — a bucket's own head can be
    /// `< target` while a later entry in the same bucket is `>= target`, so
    /// only the head comparison can be binary-searched, not the exact
    /// boundary. Used by `IndexView::compact` to place a small in-memory
    /// delta addition against the (potentially huge) on-disk base sort order
    /// without decoding every base name.
    pub fn lower_bound_name(&self, target: &[u8]) -> u32 {
        let n = self.parents.len() as u32;
        let num_buckets = self.bucket_offsets.len().saturating_sub(1);
        if num_buckets == 0 {
            return n;
        }

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
        let start_bucket = lo.saturating_sub(1);

        let mut name_buf = Vec::new();
        for b in start_bucket..num_buckets {
            let bstart = (b * BUCKET_SIZE) as u32;
            let bend = bstart + (BUCKET_SIZE as u32).min(n - bstart);
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
            if ascii::cmp_ci(&name_buf, target) != std::cmp::Ordering::Less {
                return bstart;
            }

            for rel in 1..(bend - bstart) {
                let shared = read_varint(bucket_blob, &mut p) as usize;
                let suffix_len = read_varint(bucket_blob, &mut p) as usize;
                let suffix_start = p;
                p += suffix_len;
                name_buf.truncate(shared);
                name_buf.extend_from_slice(&bucket_blob[suffix_start..suffix_start + suffix_len]);
                if ascii::cmp_ci(&name_buf, target) != std::cmp::Ordering::Less {
                    return bstart + rel;
                }
            }
        }
        n
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
