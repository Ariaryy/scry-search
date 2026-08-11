# Design decisions

## Name order plus a separate DFS permutation

Names remain sorted for binary prefix search and front coding. Tree algorithms
use separate DFS columns. This costs one permutation in each direction but
avoids giving up either lookup locality or constant-time subtree intervals.

## Parallel columns and memory mapping

Parents and DFS mappings are hot; sizes, times, subtree ends, and recursive
prefix sums are cold. Parallel mapped columns let the OS page unused metadata
out and keep idle private memory small.

## Delta overlay instead of rebuilding on every event

Most journal bursts are tiny relative to the base. Publishing base and delta as
one coherent view makes updates cheap while preserving lock-free reads. The 5%
compaction threshold is measured and intentionally favors fewer full-base
writes under the harsh compute budget.

## Streaming compaction

Compaction writes file-backed columns and builds DFS in a second spool-backed
pass. Mapped dirty pages affect working set, not private heap commit. This is
more complex than building an owned arena, but removes index-size-proportional
private-memory spikes.

## Bounded top-k and delayed paths

Selection keeps `threads * limit` keys, then reconstructs at most `limit` paths.
This prevents common one-byte queries from allocating one object per match and
gives UI consumers a cheap `Hit` API.

## RPC plus optional local execution

Named-pipe RPC is the correctness baseline. A validated read-only shared section
removes daemon round trips for steady clients; any sharing failure falls back to
RPC. This is capability negotiation over IPC—RPC is an interaction model, not a
replacement transport.

## Per-user elevated daemon, not a service by default

Index state and clients are user-scoped. A Windows service would require a
system identity, per-session authorization, impersonation or duplicated state,
and more installer recovery logic. Prefer an explicit UAC-backed per-user
startup task unless multi-user measurements justify that complexity.

## Hand-written Win32 boundary

Only the required calls and layouts are declared. This keeps dependencies and
binary size down and correctly models variable-length USN records, at the cost
of requiring careful checked parsing and focused ABI tests.

