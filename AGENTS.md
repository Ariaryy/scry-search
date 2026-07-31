## Conventions

- No `windows-sys`/`winapi` — all Win32 FFI is hand-rolled (`mod ffi` in `scry-fsevents/src/windows.rs`
  and `scry-ipc/src/ffi.rs`). `USN_RECORD_V2` has a variable-length trailing filename that doesn't
  map cleanly onto generated fixed-size bindings, and pulling in a binding crate for a dozen
  functions isn't worth it. Follow the same pattern for any new Win32 calls: minimal `extern "system"`
  block, only the constants actually used, `#[repr(C)]` structs matching the real layout.
- `ArchivedArena` (the rkyv zero-copy view) and `Arena`/`FileRecord` (the builder-side owned types)
  are different generated types. Anything that needs to work on both (e.g. `is_dir()`) needs an impl
  on each — see `record.rs`.
- Struct-of-arrays: `Arena.records` is a flat `Vec<FileRecord>`; `Arena.name_order` is a separately
  sorted permutation of indices for binary-search prefix queries. Don't fold sorting into the record
  struct itself.
- The daemon's snapshot file (`%TEMP%\scry-index-<vol>.rkyv`) lives on the volume being watched.
  Any code that writes to disk from within the daemon must be accounted for in the USN-event filter
  in `reindex_on_changes` (`scry-daemon/src/main.rs`) or it'll retrigger its own reindex.

## Known limitations (not oversights — documented tradeoffs)

- **Full reindex on change, not incremental patching.** `scryd` debounces USN events (500ms drain)
  then rebuilds the whole `Arena` and atomically swaps the mmap. rkyv's archived view is read-only
  by construction (that's what makes it zero-copy), so incremental patching needs a mutable overlay
  layered on top of the base snapshot — worth doing once reindex latency on large volumes matters.
- **Substring search is a linear scan.** Fine up to a few million entries; an n-gram index is the
  natural next step if that stops being true.
- **No `size` field populated from MFT enumeration.** USN records don't carry file size; `FileRecord.size`
  is currently always 0. Needs a lazy stat pass or `$STANDARD_INFORMATION`/`$DATA` attribute parsing.
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
