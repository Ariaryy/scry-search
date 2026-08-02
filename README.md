# scry

Instant file search for Windows. Enumerates the NTFS MFT directly (`FSCTL_ENUM_USN_DATA`)
instead of walking directory trees, keeps a zero-copy mmap'd index, and serves queries to
any client over a local named pipe.

## Components

- `scry-core` — index data structures (`Arena`), rkyv on-disk format, query engine (path terms, prefix, substring, wildcard, regex), wire protocol.
- `scry-fsevents` — MFT bulk enumeration + live USN journal watching (Windows-only).
- `scry-ipc` — named pipe framing shared by daemon and client.
- `scry-daemon` (`scryd`) — indexes a volume, serves queries, reindexes on change.
- `scry-client` — Rust SDK for talking to `scryd`.
- `scry-cli` (`scry`) — command-line query tool built on `scry-client`.

## Build

```
cargo build --release
```

### Building the daemon for release

```
cargo build --profile daemon-release -p scry-daemon
```

The `daemon-release` profile adds fat LTO, `panic = "abort"` and symbol
stripping on top of `release`. Use plain `cargo build --release` for
development; the profile difference only matters for packaging.

## Run

```
target\release\scryd.exe C:      # daemon; needs elevation to read the MFT/USN journal
target\release\scry.exe notepad  # client; does not need elevation
```

`scryd` listens on `\\.\pipe\scry`. The pipe's DACL explicitly grants local
read/write to `Everyone` so an elevated daemon stays reachable from unelevated
clients — see `scry-ipc/src/lib.rs`.

## Query syntax

Bare CLI input is split into case-insensitive, AND-ed literal terms that may
match anywhere in the full path. Double quotes keep spaces inside one term.
`*` or `?` automatically selects wildcard matching; explicit `--prefix`,
`--substring`, and `--wildcard` flags override inference.

```text
scry projects reports
scry '"annual report"'
scry '*.pdf'
scry 'report.*'
```

One-shot queries use daemon RPC by default. `--shared-index` explicitly opts
into local search over the shared snapshot.

## Roadmap status

The path-term engine is complete. Bounded top-k result materialization and the
CLI/protocol/client subset of the interactive query work are implemented.
Incremental refinement, cancellation, the daemon query pool, multi-volume
indexing, journal-resumed startup, and indexing I/O throttling remain pending.

See `docs/` for index format and protocol details, and `AGENTS.md` for
conventions and known limitations.
