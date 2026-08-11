# Index format v6

`scry-core::Arena` (`scry-core/src/arena.rs`):

```rust
Arena {
    format_version: u32,     // = 6
    names: Vec<u8>,          // front-coded name blob
    bucket_offsets: Vec<u32>,// len = num_buckets + 1; last entry == names.len()
    parents: Vec<u32>,       // bit 31 = is_dir; bits 0..30 = parent index (HOT)
    mtimes: Vec<u32>,        // seconds since 1970-01-01 UTC, clamped to [0, 2^32-1] (COLD)
    sizes: Vec<u32>,         // logical size in KiB, rounded up, saturating at u32::MAX (COLD)
    trigram_index: Vec<u8>,  // trigram-to-1024-record-block bitmap matrix
}
```

`parents`, `mtimes`, and `sizes` are index-parallel: entry `i` is described
by `parents[i]`, `mtimes[i]`, and `sizes[i]`. They are kept separate (rather
than interleaved into a struct) for a hot/cold split:

- `parents` is the hot column, touched by `full_path` on every displayed
  result. Packing it alone means a 64-byte cache line holds 16 useful parent
  hops instead of 5 (as it would interleaved with mtime and size).
- `mtimes` and `sizes` are cold columns read only by delta metadata lookup
  and compaction, never on the query path. Kept separate so the OS can evict
  those pages between compaction bursts.

## Changelog

- v6 — split the interleaved 12-byte record into three parallel 4-byte columns:
  `parents` (hot), `mtimes` (cold), `sizes` (cold). No bytes added; same
  fields, same widths, same semantics.
- v5 — added KiB-quantised logical file sizes. The raw MFT path populates the
  field; the USN fallback leaves it 0, so 0 means unknown rather than empty.
- v4 — added the trigram block filter.
- v3 — mtime_secs rebased from the 1601 FILETIME epoch to the Unix epoch (v2 saturated for all real timestamps).

Serialized with rkyv (`scry_core::store::save`) to a single file: an atomic write via
`.tmp` + rename. Reading (`ArenaStore::open`) mmaps the file and casts the bytes directly
to `&ArchivedArena` — no deserialization step, validated once via `check_archived_root`
(bytecheck) at open time, never again on the query path.

## Name blob encoding
Bucket b covers record indices [b*32, min(n, (b+1)*32)).
At byte offset bucket_offsets[b]:
- First name: varint(len) || bytes
- Each subsequent name: varint(shared) || varint(suffix_len) || suffix_bytes
  where shared = length of common prefix with PREVIOUS name in same bucket

varint = unsigned LEB128 (7 bits per byte, high bit = more follows)

## Sort order
ASCII-case-insensitive byte comparison, ties broken by original insertion index.

## Parent resolution

`WindowsBackend::bulk_index_volume` enumerates the MFT in on-disk order, which is not
tree order — a child can appear before its parent. Resolution is two passes:

1. Push every `RawEntry` via the `ArenaBuilder::push(name, mtime, is_dir)`.
2. Walk again, calling `builder.set_parent(idx, p)`.

Every entry whose parent doesn't resolve is parented to a synthesized volume-root entry (e.g. named `C:`).
This keeps `full_path()` finite (no cycles) and keeps the drive letter in
every returned path.

## Queries

`ArchivedArena::prefix_range` binary-searches bucket heads, then scans for the range — O(log n + k).
Substring queries of at least three bytes use the trigram block filter below. Shorter
substring queries and all regex queries scan the name blob sequentially. Regex is compiled
once via `regex-automata`'s `meta::Regex` with case-insensitive syntax.

## Trigram block filter

The filter maps every three-byte window of every ASCII-lowercased name to one of
16,384 rows. Blocks contain 1,024 consecutive records. Each row occupies
`ceil(num_blocks / 8)` bytes in `trigram_index`; bit `b` is stored least-significant-bit
first and is set when any name in block `b` contains a trigram mapped to that row.

The exact hash, which is part of the format, is:

```text
k = lower(a) << 16 | lower(b) << 8 | lower(c)
row = (k * 2654435761 >> 18) & 16383  // wrapping u32 multiplication
```

At query time, duplicate hashes are removed and their rows are bitwise-ANDed. Set bits
identify candidate blocks, whose names are then decoded and checked for the complete
substring. Hash collisions can add candidate blocks but cannot hide a match.

## FRN sidecar

Each snapshot may have a sibling `.frn` file containing a sorted array of
16-byte `(frn: u64, record_index: u32, padding: u32)` entries. The sidecar is a
plain native-endian POD array rather than an rkyv archive. It is mmap-backed,
validated for entry size and alignment when opened, and consulted only while
applying structural filesystem events. A missing or malformed sidecar disables
incremental updates but does not prevent the snapshot itself from opening.

The in-memory delta is never serialized. It contains tombstoned base indices
and records added after the snapshot was built. It starts empty after every
daemon restart and is merged into a new base when its change count exceeds 5%
of the base; that merge reads the existing snapshot sequentially and does not
enumerate the filesystem.

`full_path(idx, sep)` walks the parent chain from a record up to the volume root, collecting
names, then joins them in reverse.
