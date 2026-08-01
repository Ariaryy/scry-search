## Conventions

- No `windows-sys`/`winapi` — all Win32 FFI is hand-rolled (`mod ffi` in `scry-fsevents/src/windows.rs`
  and `scry-ipc/src/ffi.rs`). `USN_RECORD_V2` has a variable-length trailing filename that doesn't
  map cleanly onto generated fixed-size bindings, and pulling in a binding crate for a dozen
  functions isn't worth it. Follow the same pattern for any new Win32 calls: minimal `extern "system"`
  block, only the constants actually used, `#[repr(C)]` structs matching the real layout.
- `ArchivedArena` (the rkyv zero-copy view) and `Arena`/`FileRecord` (the builder-side owned types)
  are different generated types. Anything that needs to work on both (e.g. `is_dir()`) needs an impl
  on each — see `record.rs`.
- `Arena` uses a format v5 index (12-byte records, name-sorted, front-coded names) rather than plain `String` fields, reducing snapshot size significantly.
- The `$MFT` parser (`scry-fsevents/src/mft/`) reads a live on-disk structure
  and must treat every length and offset as hostile: checked slicing only, no
  unchecked arithmetic, and an explicit termination guard on every loop driven
  by an on-disk length. Extend `parser_never_panics_on_mutated_records` whenever
  the parser learns a new structure.
- The daemon's snapshot file (`%TEMP%\scry-index-<vol>.rkyv`) lives on the volume being watched.
  Any code that writes to disk from within the daemon must be accounted for in the USN-event filter
  in `reindex_on_changes` (`scry-daemon/src/main.rs`) or it'll retrigger its own reindex.

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
- **Substring search uses a trigram block filter** (16,384 rows × one bit per
  1024-record block, ~2 MB at a million entries). Candidate blocks are the AND
  of the needle's trigram rows; needles shorter than 3 bytes fall back to a full
  scan. Regex/wildcard queries are **not** filtered — that needs required-literal
  extraction from the pattern and hasn't been done.
- **`size` is populated only by the raw `$MFT` reader**, in KiB (rounded up,
  saturating at approximately 4 TiB). The `FSCTL_ENUM_USN_DATA` fallback leaves
  it 0 because USN records do not carry size, so 0 means unknown rather than
  empty. The raw reader is used for a supported elevated NTFS volume and
  demotes to USN on parse errors or excessive torn records.
- **Single volume per daemon instance**, chosen via argv. No multi-volume aggregation yet.
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
