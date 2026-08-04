## Conventions

- No `windows-sys`/`winapi` — all Win32 FFI is hand-rolled (`mod ffi` in `scry-fsevents/src/windows.rs`
  and `scry-ipc/src/ffi.rs`). `USN_RECORD_V2` has a variable-length trailing filename that doesn't
  map cleanly onto generated fixed-size bindings, and pulling in a binding crate for a dozen
  functions isn't worth it. Follow the same pattern for any new Win32 calls: minimal `extern "system"`
  block, only the constants actually used, `#[repr(C)]` structs matching the real layout.
- `ArchivedArena` (the rkyv zero-copy view) and `Arena` (the builder-side owned type)
  are different generated types. Anything that needs to work on both (e.g. `is_dir()`,
  `parent()`, `mtime()`) needs an impl on each — the accessors on `ArchivedArena` read
  directly from the archived column vectors. `FileRecord` no longer exists; the dual-impl
  pattern now applies to functions on `ArchivedArena` vs the free functions in `record.rs`.
- `Arena` uses a format v9 index (parallel 4-byte columns: `parents` hot,
  `mtimes`/`sizes`/`dfs_positions`/`dfs_records`/`dfs_ends` cold, plus the 8-byte
  `dfs_size_prefix` cold column; plus name-sorted front-coded names) rather than plain
  `String` fields or a single interleaved struct. The hot/cold split is load-bearing:
  `parents` alone in its column gives 16 parent hops per cache line; keeping the rest
  separate lets them stay paged out between compaction bursts. A future change that
  folds any cold field back into the hot column must update both this note and the doc
  comments in `arena.rs`.
- Records are stored **name-sorted** — `prefix_range`'s binary search and front-coding
  both depend on it — so the `dfs_*` columns (`scry-core/src/dfs.rs`) carry tree order
  separately: `dfs_positions[r]..dfs_ends[r]` is the half-open span of `dfs_records`
  holding everything beneath `r`. That makes descendant tests and descendant counts
  O(1). The parent column comes off a live volume and may contain cycles, self-parents
  and dangling indices; `dfs::build` still assigns every record exactly one position
  (cycle members become pseudo-roots), so `dfs_positions` is always a total permutation.
  The traversal is iterative on purpose — a recursive DFS overflows the stack on a
  deep or corrupt parent chain.
- **Recursive directory sizes are a prefix sum over `sizes`, laid out in the same
  depth-first order as the `dfs_*` columns** (`dfs_size_prefix` in `arena.rs`,
  built by `dfs::prefix_sums_u64`): a directory's recursive size is one subtraction,
  `dfs_size_prefix[dfs_ends[r]] - dfs_size_prefix[dfs_positions[r]]`
  (`ArchivedArena::recursive_size_kib`), not an aggregation pass. The column is `u64`
  even though each per-record value is a saturating `u32` KiB count, because a
  directory's recursive total can exceed `u32::MAX` KiB (~4 TiB) on a large volume
  well before any single file could; the ranking key still saturates the *result* to
  `u32` to fit `rank::largest_key`. A record with unknown size (see the `size` note
  below) contributes zero to every ancestor's sum, so a directory total is a lower
  bound, not an exact figure, whenever any descendant's size is unknown. Cold column,
  same eviction rationale as `mtimes`/`sizes`: 8 bytes/record, ~8 MB per million
  records in the snapshot.
- The `$MFT` parser (`scry-fsevents/src/mft/`) reads a live on-disk structure
  and must treat every length and offset as hostile: checked slicing only, no
  unchecked arithmetic, and an explicit termination guard on every loop driven
  by an on-disk length. Extend `parser_never_panics_on_mutated_records` whenever
  the parser learns a new structure.
- The daemon's snapshot file (`%LOCALAPPDATA%\scry\index-<vol>.rkyv`) lives under the user profile,
  not on the volume it describes — so with more than one volume indexed, every volume's snapshot can
  physically share one hosting drive (whichever drive the profile is on). `SelfWriteFilter` in
  `scry-daemon/src/main.rs` accounts for this two ways: `FSCTL_MARK_HANDLE` marking is applied
  against the snapshot's *hosting* volume (`hosting_volume`), and the name-based fallback for a given
  watched volume recognizes every indexed volume's snapshot filenames that are hosted there
  (`owned_snapshot_names`), not just the filenames it writes for itself. Any new on-disk write from
  within the daemon must be accounted for the same way or it'll retrigger a reindex of whichever
  volume actually hosts the write.

## Known limitations (not oversights — documented tradeoffs)

- **Incremental updates via an in-memory delta layer.** Structural USN events
  are applied to a `Delta` (a bitset of tombstoned base indices plus a `Vec` of
  added records) and published together with the base through one
  `ArcSwap<IndexView>` — never two, or a reader could pair a new base with a
  stale delta. The delta is compacted into a new base by streaming merge (no
  MFT re-enumeration) once it exceeds 5% of base size. Full reindex remains the
  fallback whenever an event can't be applied confidently, whenever the event
  channel overflowed, and on startup. Deletes use an FRN-to-index `.frn`
  sidecar kept out of the hot mmap so it stays evicted between bursts.
- **Client-local queries use an anonymous read-only section** for the immutable
  base plus a serialized delta overlay in the same generation response. Handle
  duplication targets the PID reported by `GetNamedPipeClientProcessId`; clients
  receive only `FILE_MAP_READ | SECTION_QUERY`, validate every fresh mapping,
  and fall back to RPC when sharing is unavailable. Path-term discriminant 3 is
  reserved; the internal share request uses 4.
- **Path-term queries publish their derived `PathIndex` atomically with base and
  delta.** It densely numbers directories with rank over `dir_bits`, propagates
  term masks parent-before-child, and is rebuilt for every delta publication.
  Never intersect trigram candidate blocks across terms: different terms may
  be satisfied by different ancestor records.
- **Substring search uses a trigram block filter** (16,384 rows × one bit per
  1024-record block, ~2 MB at a million entries). Candidate blocks are the AND
  of the needle's trigram rows; needles shorter than 3 bytes fall back to a full
  scan. Regex/wildcard queries use bounded must-contain HIR analysis and
  evaluate AND-of-OR literal clauses directly over the same rows. Patterns
  without a provable ASCII literal of at least 3 bytes still scan linearly.
- **Ranking is one `u64` sort key per candidate** (`scry-core/src/rank.rs`), not a
  comparator or a trait object: the bounded top-k heap compares plain integers, and every
  key carries the record in its low 32 bits so the order is total and the record reads back
  out. Descending orderings store the field's bitwise complement. Adding an `Order` variant
  means adding a `*_key` constructor and a wire discriminant — the search loops don't change.
  `Order::needs_metadata` is what lets `Relevance` skip the cold `mtimes`/`sizes` reads and
  the other orderings skip the name decode.
- **Matching, ranking and path reconstruction are three separate steps.** `search_hits`
  returns `Hit`s (record, size, mtime, is_dir); `full_path` costs a parent-chain walk and a
  `String` (~3.5 µs) and happens only in `materialize_hits`. A consumer that counts,
  aggregates, or renders lazily should stop at `Hit`. The daemon's cross-volume merge
  (`rank_sort_truncate`/`merge_rank`) rebuilds a key from `ResultEntry` fields instead —
  record indices from different volumes are not comparable.
- **`scry --verbose` prints in-process CLI phase timings** for argument parsing,
  connection, query round trip, and result printing. They begin in `main`, so exclude
  process startup/loader and teardown. A local two-volume, ~2.7M-record measurement found
  arguments at ~20–30 µs, pipe connection at ~55–65 µs, and printing at ~1–2 ms;
  the RPC query dominated at ~100–170 ms. Process wall time added ~20–25 ms outside
  `main`. A no-match path-term query still took ~34 ms, consistent with its fixed
  directory-closure pass. Treat these as diagnostic local measurements, not a regression
  benchmark; do not try to remove that closure cost as a small CLI optimization.
- **The refinement cache is keyed on the ordering as well as the terms.** A cached candidate
  set is only a superset of a refined query's matches under the *same* ordering; the scan
  keeps the best `REFINEMENT_CACHE_CAP` by that ordering and a different one would have kept
  different records.
- **`size` is populated only by the raw `$MFT` reader**, in KiB (rounded up,
  saturating at approximately 4 TiB). The `FSCTL_ENUM_USN_DATA` fallback leaves
  it 0 because USN records do not carry size, so 0 means unknown rather than
  empty. The raw reader is used for a supported elevated NTFS volume and
  demotes to USN on parse errors or excessive torn records. `MftEnumReport`
  (`scry-fsevents/src/mft/mod.rs`) splits `size == 0` into
  `files_with_unknown_size` (no unnamed `$DATA` attribute was ever found —
  a coverage gap) and `files_with_confirmed_empty_size` (an unnamed `$DATA`
  attribute was found and reported zero length — a genuine empty file), since
  conflating the two makes the coverage number meaningless. On the reference
  C: measurement the two were 1.38% and 1.36% of entries respectively — nearly
  even, and both far larger than the ~0.08% (1,540 of 2,003,924) explained by
  unresolved `$ATTRIBUTE_LIST`s; on D: they were 0.93% and roughly 0.9%. Most
  of `files_with_unknown_size` is therefore not attribute-list residue — the
  actual cause of the remainder is still open.
- **Non-resident `$ATTRIBUTE_LIST` streams are detected but not decoded.** The
  reference C: measurement reported 4,220 such lists, 1,540 unresolved base
  records, and a stable 1,555–1,564 USN-only residual versus 18–25 raw-only
  entries. This is a systematic coverage gap, not live-volume churn.
- **One daemon indexes every accessible fixed NTFS volume**, or the volumes named on argv.
  Each volume gets its own `IndexView`, watcher, and snapshot; queries fan out and merge
  through the same bounded top-k heap used within a single volume. Initial indexing is
  serialized across volumes to bound peak RSS and disk load; steady-state watching is
  concurrent. Peak-RSS and idle-reindex measurements across volumes are still outstanding.
- **C ABI export for the SDK is not implemented.** `scry-client` is Rust-only for now; non-Rust
  consumers would need a `cdylib` shim over `scry-ipc`'s framing.
- **`daemon-release` Cargo profile exists but isn't wired to any build script** — `scryd` currently
  builds with the default `release` profile. Switch the release workflow to
  `cargo build --profile daemon-release -p scry-daemon` when packaging.

## Testing notes

- `scry-fsevents` MFT/USN tests (`tests/enumerate_c.rs`, `tests/watch_journal.rs`) need elevation to
  do anything beyond confirming the "access denied without elevation" path — they pass either way,
  printing which path they took.
- Elevation in this dev environment means spawning a separate elevated PowerShell process
  (`Start-Process -Verb RunAs`), since the working shell itself isn't elevated. Use absolute paths
  for `-FilePath` — a relative path against a directory containing `[...]` in its name fails wildcard
  resolution.
