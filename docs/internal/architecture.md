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

