# Index format

`scry-core::Arena` (`scry-core/src/arena.rs`):

```
Arena {
    records: Vec<FileRecord>,   // flat, insertion order = MFT enumeration order
    name_order: Vec<u32>,       // indices into records, sorted by lowercase name
}

FileRecord {
    parent: u32,   // index into records, or u32::MAX for "no parent"
    name: String,
    size: u64,
    mtime: i64,    // Windows FILETIME
    flags: EntryFlags,   // File | Directory
}
```

Serialized with rkyv (`scry_core::store::save`) to a single file: an atomic write via
`.tmp` + rename. Reading (`ArenaStore::open`) mmaps the file and casts the bytes directly
to `&ArchivedArena` — no deserialization step, validated once via `check_archived_root`
(bytecheck) at open time, never again on the query path.

## Parent resolution

`WindowsBackend::bulk_index_volume` enumerates the MFT in on-disk order, which is not
tree order — a child can appear before its parent. Resolution is two passes:

1. Push every `RawEntry` as a `FileRecord` with `parent = u32::MAX`, building
   `HashMap<frn, index>` as you go.
2. Walk again, looking up each entry's `parent_frn` in the map and calling `set_parent`.

Every entry whose parent doesn't resolve (the true NTFS root is self-parented, and some
enumeration quirks leave it out entirely) is parented to a synthesized volume-root
`FileRecord` (e.g. named `C:`) pushed once per `bulk_index_volume` call, rather than left
as `u32::MAX`. This keeps `full_path()` finite (no cycles) and keeps the drive letter in
every returned path.

## Queries

`ArchivedArena::prefix_range` binary-searches `name_order` via `partition_point` — O(log n).
Substring and regex queries (`scry_core::query::search`) are a linear scan over `records`;
regex is compiled once via `regex-automata`'s `meta::Regex` with case-insensitive syntax.

`full_path(idx, sep)` walks the parent chain from a record up to the volume root, collecting
names, then joins them in reverse.
