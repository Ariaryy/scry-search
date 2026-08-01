# Index format v3

`scry-core::Arena` (`scry-core/src/arena.rs`):

```rust
Arena {
    format_version: u32,     // = 3
    names: Vec<u8>,          // front-coded name blob
    bucket_offsets: Vec<u32>,// len = num_buckets + 1; last entry == names.len()
    records: Vec<FileRecord>, // 8 bytes each, in name-sorted order
}

FileRecord {
    parent_and_flags: u32,   // bit 31 = is_dir; bits 0..31 = parent record index
    mtime_secs: u32,         // seconds since 1970-01-01 UTC, clamped to [0, 2^32-1] (year 2106)
}
```

Format v3 is an 8-byte record compact index, name-sorted storage, front-coded name blob.

## Changelog

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

Every entry whose parent doesn't resolve is parented to a synthesized volume-root `FileRecord` (e.g. named `C:`).
This keeps `full_path()` finite (no cycles) and keeps the drive letter in
every returned path.

## Queries

`ArchivedArena::prefix_range` binary-searches bucket heads, then scans for the range — O(log n + k).
Substring and regex queries (`scry_core::query::search`) are a linear scan over `records` and the name blob sequentially;
regex is compiled once via `regex-automata`'s `meta::Regex` with case-insensitive syntax.

`full_path(idx, sep)` walks the parent chain from a record up to the volume root, collecting
names, then joins them in reverse.
