# Scry Search agent guide

Read [`docs/internal/architecture.md`](docs/internal/architecture.md),
[`design-decisions.md`](docs/internal/design-decisions.md), and the relevant
format/protocol document before changing a load-bearing boundary. Historical
measurements belong in internal docs, not this always-loaded file.

## Non-negotiable resource budget

- Performance is correctness. Idle CPU and disk I/O should approach zero.
- Bound private memory independently of index size where streaming or mapped
  storage is possible. Do not replace spool-backed compaction with an owned arena.
- Never allocate/materialize per match for a bounded top-k query. Retain keys,
  then construct at most `limit` paths.
- Normalize query terms once per request; never case-fold or allocate inside an
  up-to-20,000-hit refinement loop.
- Any performance claim needs build mode, corpus, query shape, sample count, and
  before/after numbers. Keep benchmark vocabulary generic and synthetic.

## Storage and tree invariants

- Current snapshot format is v10. Names are ASCII-CI name-sorted and front-coded.
  `prefix_range` depends on that order.
- Keep hot columns (`parents`, `dfs_positions`, `dfs_records`) separate from cold
  metadata (`mtimes`, `sizes`, `size_exact_bits`, `dfs_ends`,
  `dfs_size_prefix`). Update `arena.rs` docs and
  [`docs/internal/index-format.md`](docs/internal/index-format.md) if layout changes.
- `Arena` and rkyv's `ArchivedArena` are different types. Shared accessors need
  implementations for both (or the established free-function equivalent).
- Raw parents may cycle, self-parent, or dangle. DFS is iterative and assigns a
  total permutation. All ancestry/path logic uses `tree_parent`, never raw
  `parent`, so paths and subtree intervals agree on corrupt input.
- `dfs_positions[r]..dfs_ends[r]` is the canonical subtree in `dfs_records`.
  Recursive sizes are one `u64` prefix subtraction. Unknown descendants
  contribute zero and make the directory total a lower bound.
- Format changes require validation updates, byte-identical producer coverage,
  protocol/shared-section review, and exactly one intentional version bump.

## Updates, persistence, and queries

- Publish immutable base plus live delta through one `ArcSwap<IndexView>`; never
  publish them independently.
- Keep the measured 5% compaction threshold unless a maintained real trace
  disproves its cost model. Compaction must remain spool/file-backed, preserve
  snapshot/FRN generation pairing, and clean scratch files on every exit path.
- Account for every daemon-created file in `SelfWriteFilter`, including files
  physically hosted on another watched volume. Otherwise the daemon can index
  its own writes.
- Path-term queries operate in DFS-position space. Build an independent interval
  set per term; never intersect raw trigram candidate blocks across terms.
- Metadata filters run before top-k retention for base and delta. Do not emulate
  them by filtering a truncated result page in a client.
- Ranking is one total `u64` key carrying the local record. Cross-volume merge
  uses rank bits plus stable volume slot; local record indices are not globally
  comparable.
- Matching, ranking, and materialization remain separate. `Hit` should stay 24
  bytes unless measurements justify growth.

## IPC and privilege boundary

- RPC uses the local named pipe; shared-index execution is an optional read-only
  acceleration with automatic RPC fallback.
- Shared handles are duplicated only to the PID reported by the pipe and with
  read/query rights. Validate every new mapping and overlay.
- If an announced replacement mapping fails, discard the stale local generation
  and retry the handshake immediately; only RPC may answer meanwhile.
- Cache shared sections by monotonic `IndexView::generation`, never allocator
  address (ABA hazard).
- Keep wire additions bounded and versioned. Client and daemon releases are
  currently compatibility-coupled.
- `scryd` requires elevation for complete raw NTFS indexing. Clients remain
  unelevated. Deployment uses an explicit UAC-backed, highest-privilege per-user
  logon task, not a system service.

## Windows/filesystem safety

- Do not add `windows-sys` or `winapi`. Follow the minimal hand-written Win32 FFI
  pattern in `scry-fsevents/src/windows.rs` and `scry-ipc/src/ffi.rs`.
- Treat all MFT/USN lengths and offsets as hostile: checked slicing/arithmetic
  and an explicit termination guard for every on-disk-length loop. Extend the
  mutation no-panic test when parsing new structures.
- Raw MFT supplies size provenance; USN enumeration does not. Zero without an
  exactness bit means unknown, not necessarily empty. Non-resident attribute-list
  streams remain a documented coverage gap.
- Never add personal paths, filenames, identifiers, or scrubbed PII to tracked
  source, tests, fixtures, docs, logs, or benchmarks.

## Verification and commits

Before completion:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

MFT/USN live-volume tests require elevation; they otherwise verify the denied
path. In this environment launch an elevated PowerShell with absolute paths.

Use plain-language Conventional Commits:

```text
fix(<feature>): <message>
feat(<feature>): <message>
docs(<feature>): <message>
```

Do not hard-code user filesystem paths. Do not merge or push without explicit
authorization; the standing fast-forward instruction applies only after every
done criterion is verified.

## Current public limitations

- Rust client only; no stable C ABI yet.
- Snapshot and IPC formats may change during `0.1.x` alpha releases.
- Complete indexing targets elevated fixed NTFS volumes on Windows 10/11.
- `daemon-release` is the packaging profile; ordinary development uses release.

