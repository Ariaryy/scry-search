# Index format v10

The snapshot is a validated, memory-mapped rkyv archive. Records are sorted by
ASCII-case-insensitive leaf name; front coding and prefix search depend on that
order. Tree order is stored separately.

## Archive columns

| Field | Width | Purpose |
|---|---:|---|
| `format_version` | 4 bytes | Reject incompatible snapshots before reading fields. |
| `journal_id`, `next_usn`, `volume_serial` | 24 bytes total | Resume and validate the volume journal cursor. |
| `names` | variable | Front-coded UTF-8 name bytes. |
| `bucket_offsets` | 4 bytes/bucket | Random entry into 32-record name buckets, plus sentinel. |
| `parents` | 4 bytes/record | Packed raw parent and directory bit. |
| `mtimes` | 4 bytes/record | Unix seconds. |
| `sizes` | 4 bytes/record | Logical KiB, rounded up and saturating. |
| `size_exact_bits` | 1 bit/record | Distinguishes measured zero from unknown/incomplete size. |
| `trigram_index` | workload-dependent | One bit per 1,024-record block in each of 16,384 rows. |
| `dfs_positions` | 4 bytes/record | Name record to canonical DFS position. |
| `dfs_records` | 4 bytes/record | DFS position to name record. |
| `dfs_ends` | 4 bytes/record | Exclusive canonical subtree end. |
| `dfs_size_prefix` | 8 bytes/record + sentinel | Recursive-size prefix sum in DFS order. |

`parents`, `dfs_positions`, and `dfs_records` are hot query columns. Metadata,
subtree ends, and size prefixes are separate so Windows can evict their pages
when queries do not need them.

## Names and lookup

Bucket `b` covers records `[b*32, min(n, (b+1)*32))`. The first name stores
`varint(length) + bytes`; later names store `varint(shared_prefix) +
varint(suffix_length) + suffix`. Varints are unsigned LEB128. Prefix lookup
binary-searches bucket heads and scans the bounded edge buckets.

Sequential query walkers can reuse name-derived match results when a front-coded
entry has `shared_prefix == previous_name_length` and an empty suffix. Reuse
is byte-exact, local to one scan/worker, and resets at bucket and range heads.
Skipped candidates invalidate a new-name cache before any later duplicate is
emitted. Record metadata, tombstones, DFS ancestry and top-k keys are checked
independently for every record; no name copies or global dictionary are needed.

Substring and literal-bearing regex/wildcard queries use the trigram block
matrix. Hash collisions may add blocks but cannot remove a match. Short terms
and patterns without a provable literal use a cancellable sequential or
parallel bucket scan.

## Canonical tree

Live parent data may be cyclic, self-referential, or dangling. Iterative DFS
assigns every record exactly one position and forms a deterministic spanning
forest. `tree_parent` accepts a raw edge only when the child lies in the raw
parent's stored subtree. Path reconstruction and ancestor matching must use
that canonical edge, so materialized paths and interval queries agree even on
corrupt input.

`dfs_positions[r]..dfs_ends[r]` is record `r`'s subtree in `dfs_records`.
Path-term search normally converts every term to coalesced DFS intervals and
intersects the smallest sets first. For multi-term queries without delta
additions, independent trigram survival estimates choose a seed term. If its
actual subtree union covers at most 1,024 positions, canonical ancestor masks
can verify the other terms instead of building their interval sets. A fixed
2,048-slot directory memo and 512-entry walk stack use 20 KiB total; verification
permits at most 16,384 steps and falls back once decoded name lengths exceed
1 MiB (checked after each name decode). Cache collisions replace
entries by record identity. Exceeding any budget discards partial verification
and resumes independent term scans, reusing the seed set. Tombstoned names do
not contribute masks; metadata filtering and ranking follow verification.
Views with delta additions retain the interval fold for base ancestry lookup.
These query-local choices do not change the archive layout.

Recursive directory size is one prefix subtraction;
unknown descendants contribute zero and clear the directory's exactness bit.

Final path-term candidates needing names use direct decoding for tiny sets.
Larger sparse sets stage at most 4,096 record IDs (16 KiB), sort each batch,
and decode only its touched front-coded buckets. Dense sets use a temporary
record bitmap; the choice compares four bytes per survivor with one bit per
base record. This staging is query-local and does not change the archive.
Dense parallel ranking builds one such bitmap and shares it read-only across
workers, avoiding a binary search over DFS intervals for each decoded record.
The bitmap uses `ceil(base_records / 8)` bytes in total, independent of worker
count, and is released on completion or cancellation.
The serial bitmap visitor polls cancellation before every bucket, including
empty buckets, and after the final bucket. Each bucket decodes at most 32
records; this bounds traversal work between polls, not wall-clock latency.

Each term's interval set coalesces queued spans in the same allocation as its
settled runs. Reusable path-search scratch has an aggregate 1 MiB capacity
budget per execution thread; larger buffers are released after the query,
including cancelled returns. This bounds owned scratch retained between queries,
not in-flight interval storage, caller-owned results, or allocator-retained pages.

## Persistence and compaction

Full builds and delta compaction write columns through file-backed spools and
serialize directly from them; they do not materialize a second owned arena.
In a normal daemon installation, these files live under `%LOCALAPPDATA%\scry`
as `index-<volume>.rkyv` and `index-<volume>.frn`. The final snapshot is
atomically renamed. A sibling `.frn` sidecar stores
sorted `(frn: u64, record: u32, padding: u32)` entries and carries the same
snapshot-generation tag. A crash between independent renames makes a mismatched
sidecar get ignored rather than paired with the wrong snapshot.

The live delta contains tombstoned base records and additions. It is compacted
at 5% because maintained measurements show the overlay scan is much cheaper
than rewriting the full base more frequently.

## Compatibility history

- v10: one persisted size-provenance bit per record.
- v9: DFS size-prefix column and current hot/cold tree layout.
- v8–v7: DFS interval columns and streaming/tree-layout evolution.
- v6: split packed records into parallel parent, mtime, and size columns.
- v5: logical size column.
- v4: trigram block filter.
- v3: Unix timestamp epoch.

`ArenaStore::open` validates the archive once, checks v10, then exposes the
archived columns directly. Old snapshots are rebuilt, not migrated in place.

