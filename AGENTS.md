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
- `Arena` uses a format v9 index (parallel 4-byte columns: `parents`/`dfs_positions`/
  `dfs_records` hot, `mtimes`/`sizes`/`dfs_ends` cold, plus the 8-byte `dfs_size_prefix`
  cold column; plus name-sorted front-coded names) rather than plain `String` fields
  or a single interleaved struct. The hot/cold split is load-bearing: `parents` alone
  in its column gives 16 parent hops per cache line; keeping the rest separate lets
  them stay paged out between compaction bursts. `dfs_positions` and `dfs_records`
  moved from cold to hot because path-term search now reads them on every query (see
  the DFS-position-space bullet below), not just on the size-prefix build path;
  `dfs_ends` stayed cold since it's only read once per matching directory to push a
  `subtree()` span, not per candidate. A future change that folds any field between
  the hot and cold groups must update both this note and the doc comments in
  `arena.rs`.
- Records are stored **name-sorted** — `prefix_range`'s binary search and front-coding
  both depend on it — so the `dfs_*` columns (`scry-core/src/dfs.rs`) carry tree order
  separately: `dfs_positions[r]..dfs_ends[r]` is the half-open span of `dfs_records`
  holding everything beneath `r`. That makes descendant tests and descendant counts
  O(1). The parent column comes off a live volume and may contain cycles, self-parents
  and dangling indices; `dfs::build` still assigns every record exactly one position
  (cycle members become pseudo-roots), so `dfs_positions` is always a total permutation.
  A raw parent edge is part of that canonical forest only when the child's DFS position
  lies inside the raw parent's stored subtree; `Arena::tree_parent` and
  `ArchivedArena::tree_parent` apply that O(1) test. All path reconstruction and
  ancestor matching must use `tree_parent`, not the raw `parent`, so a corrupt cycle
  cannot give materialized paths ancestry that interval search cannot represent.
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
  stale delta. The delta is compacted into a new base once it exceeds 5% of
  base size, without MFT re-enumeration and without ever materializing an
  owned `Arena`: `IndexView::compact_to_snapshot` (`scry-core/src/view.rs`)
  merges base survivors and live delta additions straight into file-backed
  spools (`scry-core/src/spool.rs`) via `SpooledArenaBuilder`
  (`scry-core/src/arena.rs`), builds the `dfs_*` columns in a second pass
  over the finished (file-backed) parent column (`dfs::build_file_backed`,
  `scry-core/src/dfs.rs` — same iterative, corrupt-parent-tolerant traversal
  as `dfs::build`, just spool-backed scratch and outputs instead of `Vec`),
  serializes the same v9 archive layout straight from those spools via a
  hand-written `rkyv::Archive`/`Serialize`
  impl on `ArenaColumns` (`scry-core/src/store.rs`) that drives the same
  `ArchivedVec` primitives the derived `Arena` impl would — proven
  byte-identical to the derived path by
  `save_columns_with_matches_save_with_byte_for_byte`, then merges the FRN
  sidecar by walking the already frn-sorted base sidecar alongside the small
  sorted delta-FRN list (`FrnMap::save_streaming`, `scry-core/src/frnmap.rs`)
  instead of collecting a full-size `Vec<FrnEntry>`. The snapshot header's
  generation tag is copied into the sidecar and checked when opening, so a
  crash between the two independent renames can only make the sidecar get
  ignored; it cannot pair indices from different generations. A mutable mmap's
  dirty pages are written back to the backing file and counted by Windows
  as "Mapped File" memory (`WorkingSetSize`), not `PrivateUsage`, so this
  keeps compaction's transient heap footprint far below an owned-`Arena`
  merge regardless of index size. Every spool/scratch file lives under a
  per-volume scratch directory and is deleted on drop; each one is passed
  through the same `on_create` auxiliary-marking hook as the final snapshot
  (see the self-write accounting note above and `compaction_scratch_names`
  in `scry-daemon/src/main.rs`). Full reindex remains the fallback whenever
  an event can't be applied confidently, whenever the event channel
  overflowed, and on startup. Deletes use an FRN-to-index `.frn` sidecar
  kept out of the hot mmap so it stays evicted between bursts.
  `compact_to_snapshot` takes an `on_phase` callback fired after merge,
  dfs_build, prefix_sums, serialize, and frn_merge, purely so a caller can
  sample process memory between phases (`SCRY_COMPACTION_MEM_PROBE=1` in
  `scry-daemon`); it costs nothing when the callback is a no-op. The event burst
  is explicitly dropped after its records have been copied into `Delta`, before
  compaction starts — retaining both copies was enough to miss the absolute
  private-memory target even though the writer itself was bounded. Measured on
  an elevated dedicated NTFS test volume, adding 12,000 records to a 224,231-
  record base produced a 236,232-record snapshot in 0.846 s. Continuous external
  sampling and the phase probe agreed on a 29,229,056-byte peak `PrivateUsage`
  (only ~20 KB above compaction start); sampled `WorkingSet` peaked at 29,126,656
  bytes as mapped spool pages were touched. A subsequent ten-minute idle run
  sampled the daemon once per second: zero read/write bytes, reindexes,
  compactions, or idle-persist writes; private commit stayed 18.43–18.50 MB and
  working set 1.95–3.04 MB. This validates the absolute target and bounded growth
  at the measured scale; the old roughly 2M-record baseline was not rerun at the
  same scale, so do not present this as a like-for-like throughput comparison.
- **Client-local queries use an anonymous read-only section** for the immutable
  base plus a serialized delta overlay in the same generation response. Handle
  duplication targets the PID reported by `GetNamedPipeClientProcessId`; clients
  receive only `FILE_MAP_READ | SECTION_QUERY`, validate every fresh mapping,
  and fall back to RPC when sharing is unavailable. If an announced replacement
  mapping fails validation, the client discards its older local generation and
  leaves the handshake retry immediately due; only the RPC fallback may answer
  until a current mapping installs successfully. Path-term discriminant 3 is
  reserved; the internal share request uses 4. The daemon's shared-section cache
  (`shared_section` in `scry-daemon/src/main.rs`) is keyed on `IndexView::generation`
  and holds the keyed `Arc<ArenaStore>` alongside the cached `Arc<Section>` — do not
  key it on `Arc::as_ptr(&view.base)` again, since a freed generation's address can
  be reused by a later one and that reintroduces an ABA bug where a client is handed
  a stale mapping under a key that looks fresh.
- **Path-term queries work entirely in DFS-position space, over the columns
  the snapshot already stores** (`arena.dfs_position`/`dfs_record`/`subtree`)
  — there is no separate derived index to publish or rebuild. Each term gets
  its own `IntervalSet` (`scry-core/src/intervals.rs`) from an independent
  trigram-filtered scan: a directory match pushes its whole `subtree()` span,
  a name match pushes a single point. The answer is the intersection of all
  terms' interval sets, folded smallest-first. An empty base set short-circuits
  later term scans only when no live delta name can supply that term; otherwise
  the remaining base sets are still needed to evaluate delta records correctly.
  Delta records have no DFS position, so they're handled by a separate,
  unconditional linear walk up each addition's ancestor chain, testing base
  containment via the same interval sets. Never intersect trigram candidate
  blocks across terms: different terms may be satisfied by different ancestor
  records — this algorithm intersects the *derived* per-term interval sets in
  position space, never the raw candidate blocks themselves.
- **A path-term candidate's own name is decoded twice under `Order::Relevance`**:
  once per term while building that term's interval set (to test whether the
  candidate's own name — not an ancestor's — satisfies that specific term),
  and again during final enumeration (to compute the *combined* quality mask
  against all terms at once, since a record can satisfy some terms through its
  own name and others by inheriting an ancestor directory's match). The two
  scans test different things — a per-term substring test vs. a multi-pattern
  automaton over the whole term set — so the second decode isn't a redundant
  copy of the first; caching it would mean tracking per-record match state
  across independent per-term scans, which the design deliberately avoids.
  This is worth the cost: on a 440k-record synthetic corpus (`cargo bench -p
  scry-core --bench query -- path_terms`), rare/no-match/mixed-selectivity
  queries improved 30–72% (a no-match query dropped from ~34 ms to ~37 µs
  in-process) by dropping the old implementation's unconditional full-index
  scan and directory-closure pass. The two cases where a *single* term matches
  a large fraction of the corpus (`common`, an infix present in ~39% of
  trigram blocks; `clustered`, a prefix matching 2,048 directories) regressed
  16–31%, because those are exactly the cases where the double decode's
  constant factor dominates instead of being hidden by avoided full scans.
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
  `String` (~2.4–3.0 µs per hit, measured via the `materialize` criterion bench at 50/1000/20000
  hits on a 440k-record corpus) and happens only in `materialize_hits`. A consumer that counts,
  aggregates, or renders lazily should stop at `Hit`. The daemon's cross-volume merge
  (`rank_sort_truncate`/`merge_rank`) rebuilds a key from `ResultEntry` fields instead —
  record indices from different volumes are not comparable.
- **Daemon query spans reflect the fused top-k implementation.** `select_ns`
  covers base matching plus bounded-heap retention because substring and regex
  perform those operations in one streaming pass; calling it only "matching"
  would be misleading. `finalize_ns` covers the independently measurable live-
  delta merge and final heap drain. Path-term search reports its complete hit
  selection in `select_ns` and leaves `finalize_ns` at zero because that pipeline
  is not split at the same boundary. Materialization and encoding remain separate.
- **`scry --verbose` prints in-process CLI phase timings** for argument parsing,
  connection, query round trip, and result printing. They begin in `main`, so exclude
  process startup/loader and teardown. A local two-volume, ~2.7M-record measurement found
  arguments at ~20–30 µs, pipe connection at ~55–65 µs, and printing at ~1–2 ms;
  the RPC query dominated at ~100–170 ms. Process wall time added ~20–25 ms outside
  `main`. A no-match path-term query now costs microseconds in-process (~37 µs,
  measured via the `path_terms/no_match` criterion bench on a 440k-record corpus)
  rather than a fixed per-query pass over every directory. On the live two-volume
  index, 200 process-per-query samples of a generic absent term measured 0.853 ms
  p50 / 6.449 ms p99 query round trip; over one steady client connection the same
  query measured 0.020 ms p50 / 0.334 ms p99. A generic selective two-term query
  measured 21.449 ms p50 / 24.197 ms p99 across 200 cold connections. The early
  exit is valid only when neither the base nor a live delta name can supply the
  empty term. Treat these as diagnostic local measurements, not a regression
  benchmark.
- **The refinement cache is keyed on the ordering as well as the terms.** A cached candidate
  set is only a superset of a refined query's matches under the *same* ordering; the scan
  keeps the best `REFINEMENT_CACHE_CAP` by that ordering and a different one would have kept
  different records. Refinement terms are ASCII-normalized once per request and
  borrowed by every cached-hit predicate; do not move case folding back inside
  the up-to-20,000-hit filter loop.
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

## Misc

- Conventional commit titles, plain language: fix(<feature>): <message>
- Do not include references / hard code paths to files in the user's file system
