# Architecture

Scry separates privileged filesystem ingestion from cheap, reusable querying.

```text
NTFS metadata + USN journal
          |
        scryd
   builder -> immutable snapshot + FRN sidecar
   watcher -> bounded live delta
          |
   named-pipe RPC / read-only shared section
          |
      scry-client
     CLI or embedded application
```

## Components

- `scry-fsevents` reads raw MFT records when elevated, falls back conservatively,
  and converts journal activity into structural events.
- `scry-core` owns the archive, delta, query algorithms, ranking, compaction,
  and wire payloads. It is independent of UI concerns.
- `scry-daemon` owns volumes, persistence, watchers, publication, cross-volume
  top-k merge, RPC, and shared-section caching.
- `scry-ipc` provides minimal hand-written Win32 pipe and section primitives.
- `scry-client` provides RPC fallback, validated local search, and persistent
  realtime sessions.
- `scry-cli` is a thin formatter and interactive reference consumer.

## Source layout

Public crate APIs and snapshot/wire formats stay at their existing import paths.
Implementation modules are private and organized around the responsibility they
own. Entry points assemble the application rather than implement every subsystem.

- **Core archive:** `arena.rs` defines the layout and canonical tree access.
  `arena/names.rs` owns front coding, bucket scans, and sorted lookup;
  `arena/builder.rs` stages unsorted input; `arena/spooled.rs` constructs sorted
  columns without an owned arena. `store/columns.rs` owns borrowed-column rkyv
  serialization. `dfs/file_backed.rs` owns spool-backed traversal and prefix sums.
- **Core queries:** `query.rs` owns sequential scans and dispatch;
  `query/parallel.rs` owns bucket sharding and workers. `view.rs` exposes coherent
  base/overlay searches and materialization. `view/ranked.rs` owns filtering and
  top-k selection, `view/path_search.rs` owns independent DFS term intervals and
  bounded scratch, and `view/compaction.rs` owns streaming snapshot/FRN merging.
- **Daemon:** `main.rs` wires startup, shutdown, and workers; `options.rs` parses
  startup options; `platform.rs` applies Windows scheduling policy. `paths.rs`
  owns snapshot/spool locations, `changes.rs` classifies events and excludes
  daemon writes, `persistence.rs` builds/replays/compacts snapshots, and
  `updates.rs` coalesces journal activity and publishes views. `search.rs` owns
  cross-volume ranking and refinement; `rpc.rs` owns pipe framing, request
  dispatch, and cancellation; `sections.rs` owns the bounded generation cache.
- **Windows ingestion:** `windows.rs` exposes the backend and shared event types.
  `windows/indexing.rs` builds initial indexes, `windows/records.rs` decodes USN
  records, `windows/journal.rs` validates/replays/watches journals, and
  `windows/ffi.rs` declares the minimal Win32 boundary.
- **Consumers:** `scry-client/src/lib.rs` exposes the SDK and RPC calls;
  `local.rs` owns mapping validation and fallback, and `session.rs` owns
  interactive request draining. `scry-cli/src/display.rs` owns terminal-safe
  presentation; the CLI entry point owns argument handling.

Unit tests live beside these modules in `tests.rs` or responsibility-specific
`tests/` submodules. Shared fixtures stay in the suite parent. Keep behavioral
oracles and regressions for corrupt input, cancellation, bounded resources,
serialization identity, and fallback. Prefer extending an existing behavioral
test over duplicating its setup or testing a copied implementation formula.

## Data flow

Initial indexing is serialized across fixed NTFS volumes to bound memory and
I/O. Each volume publishes one coherent `Arc<IndexView>` containing immutable
base plus live delta. Queries take one view, so they cannot pair a new base with
an old overlay. Structural journal events update the delta; uncertain events or
overflow request a full rebuild. Five-percent delta growth triggers streaming
compaction.

Matching, ranking, and path materialization are distinct. Matching retains only
bounded top-k integer keys; paths are reconstructed only for emitted hits.
Cross-volume merge uses carried rank bits and stable volume slots, never local
record indices as globally comparable values.

## Operating model

The daemon needs elevation for complete raw metadata access, but snapshots and
IPC are per-user. Applications should connect once, reuse sessions, and degrade
gracefully when the daemon is absent. Packaging should install matching client
and daemon versions and provide an explicit UAC-backed per-user startup path.

