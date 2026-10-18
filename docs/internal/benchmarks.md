# Benchmarking

Scry measures client latency, indexing, update visibility, and daemon resource
use separately. This document describes the maintained tools and the latest
validated observations. Measurements describe a particular build and workload;
they are not performance guarantees or comparisons with other search products.

## Measurement approach

The repository's `scry-bench` tool exercises the Rust client through
named-pipe RPC and read-only shared search. Captures record the route actually
used, including RPC fallback. Query timing includes response decoding and result
path construction. Reference construction and answer validation run outside the
timer. RPC session refinement and local shared search have different execution
paths, so their timing difference is not solely transport overhead.

Every measured answer is checked for result count, order, paths, metadata and
flags. Frozen inputs provide a reference computed directly from the supplied
snapshots and overlays. This checks the daemon/client boundary against core
search; it does not independently prove the core matcher correct. Generated
workloads also recount supported match predicates exhaustively rather than
estimating selectivity from a limited result page.

Current captures use JSONL schema 3. Each sample includes an ordered-answer
SHA-256 digest; comparisons reject changed answers, incompatible context, missing
samples and incomplete runs. Older schemas remain diagnostic history. In
particular, the first live capture predates every-answer validation and is not
accepted as performance comparison evidence.

## Recorded observations

### Full daemon lifecycle

Recorded on 2026-10-18 using a `release` build at `6778633`, targeting
`x86_64-pc-windows-msvc`. The reference machine has an Intel Core i5-13500HX,
14 cores / 20 logical processors and approximately 16 GiB RAM, on AC with the
High performance power plan. The daemon indexed one fixed NTFS volume containing
931,047 records, with one query worker, refinement enabled and the default
128 MiB/s indexing read cap.

A separate elevated daemon used a new cache directory and private pipe. The
installed daemon remained running. There was no snapshot or journal resume;
OS cache and other machine activity were uncontrolled. Index time therefore
means fresh indexing through query-pipe readiness, not cold-disk performance.

| Measurement | Result |
|---|---:|
| Fresh indexing, one run | 10.762 s elapsed; 1.484 s process CPU |
| Index private-memory sampled maximum, 50 ms interval | 215.2 MiB |
| Query private-memory / working-set sampled maxima, 20 ms interval | 215.4 / 185.0 MiB |
| Disconnected idle duration after 30 s settling | 600.000 s |
| Private memory, idle start → end | 55.38 → 55.34 MiB |
| Working set, idle start → end | 59.42 → 13.68 MiB |
| Idle process CPU | 0 s at counter resolution |
| Idle logical read/write bytes and operations | 0 |
| Idle other I/O | 2,320 bytes in 26 operations |
| One and eight connected idle sessions, separate 60 s checks | 0 observed CPU and process I/O |

The query phase used six generic query shapes at limit 50, repeated five times
through each route: 30 RPC and 30 shared searches, with equal ordered results.
Its sampled maxima include memory retained shortly after indexing; they do not
measure incremental allocation caused by queries. Memory peaks are sampled lower
bounds. Private bytes measure committed private memory; working set measures
resident pages and can fall while private commit stays stable. No trimming was
forced. Idle endpoints precede filesystem visibility probes.

CPU and I/O use counters taken at the exact idle boundaries, with periodic
resource samples at one-second intervals. Process I/O counters describe logical
operations, not physical disk traffic or delayed writeback. Zero CPU at counter
resolution does not establish zero wakeups.

Ten creates and ten renames were checked through both routes, polling every
20 ms. Each check required the exact path to appear; rename also required the
old path to disappear.

| Operation | RPC median / maximum | Shared median / maximum |
|---|---:|---:|
| Create, 10 samples | 520.22 / 538.10 ms | 547.08 / 781.57 ms |
| Rename, 10 samples | 530.79 / 541.07 ms | 549.43 / 797.23 ms |

These are observed client visibility delays, including polling and querying.
Ten samples per operation do not establish a reliable tail distribution.

### Frozen query latency

An earlier validated `release` build over `682edfd`, including the benchmark and
cancellation fix subsequently committed in `34079b0`, used a frozen v10 snapshot
of 927,762 records on the same machine. It had one volume, empty delta, one query
worker, refinement enabled, result limit 50 and uncontrolled OS cache. Snapshot
hashing and reference queries preceded timing. A first request means a new
connection, not a cold machine.

Three fresh daemon processes each ran three connection rounds, two warmup
sequences and 40 recorded warm sequences per round. All 16,605 recorded answers
validated, with no route fallbacks. The ranges below span the three per-process
warm medians, each based on 120 observations per step and route. They are not
confidence intervals or before/after speedup estimates.

| Query step | RPC p50 range | Shared p50 range |
|---|---:|---:|
| One-character path term | 160.126–161.294 ms | 144.903–150.228 ms |
| Six-character replacement path term | 6.738–6.813 ms | 6.512–6.707 ms |
| Negative path-term probe | 0.176–0.199 ms | 0.115–0.118 ms |
| Typing: six-character term | 2.697–3.028 ms | 5.878–6.402 ms |
| Three-character prefix | 0.372–0.424 ms | 0.182–0.274 ms |
| Six-character substring | 7.758–7.965 ms | 6.820–7.708 ms |
| Extension filter, largest order | 60.556–61.293 ms | 57.305–58.895 ms |

Broad terms remain expensive. The earlier
[query-latency baseline](query-latency-baseline.md) retains historical core and
allocator diagnostics; its different builds and corpora must not be pooled with
these measurements.

## Reproduction

Build the development benchmark binaries from the checkout being measured:

```powershell
cargo build --release -p scry-bench -p scry-daemon
target/release/scry-bench.exe --help
```

Keep snapshots, captures, workloads and logs under ignored `.scratch/benchmark/`.
Outputs must be new files or directories. Workload generation can derive real
names from a supplied snapshot; keep those inputs private and publish only
generic query descriptions and aggregates.

For query experiments, start `scryd` with `--benchmark-snapshot FILE` and
`--benchmark-pipe NAME`, using a private `\\.\pipe\scry-bench-...` name. This
mode needs no elevation and performs no live ingestion or persistence. Supply
the same snapshot to the runner with `--snapshot FILE` and the pipe with
`--pipe NAME`. Both accept repeated snapshots and an optional overlay immediately
after its corresponding snapshot. Multi-volume execution currently reports RPC
fallback. Use `scry-bench scenario --help` to prepare synthetic volumes/overlays.

For lifecycle measurements, run the launcher in an elevated PowerShell. Set
`$volume` to the explicitly selected fixed NTFS drive designator:

```powershell
./scripts/benchmark-lifecycle.ps1 -Volume $volume -OutputDirectory .scratch/benchmark/lifecycle-new -Machine reference-windows -Power AC-high-performance
```

The launcher starts and stops its own daemon. Defaults are 600 seconds
disconnected idle after 30 seconds settling, ten creates and ten renames, then
60-second idle checks with one and eight connected sessions. Raw observations
are written after their measurement windows. `scry-bench observe --help` exposes
the collector separately.

## Corpus coverage and comparisons

`fingerprint` reports corpus size, canonical tree depth/fanout, name lengths and
repetition, extension concentration, size provenance and sampled trigram-block
survival. This is a descriptive vector, not a single difficulty score.
`synthesize` provides seeded `calibrated-v1` and `stress-v1` profiles; the latter
adds wide directories, multiple roots, Unicode/long names and unknown/empty
sizes. Calibration approximates selected distributions rather than reproducing
an entire real filesystem. `generate-workload` produces prefixes, substrings,
ancestor/leaf terms, filters, broad probes and explicit negatives with exact
count strata for supported single-snapshot predicates.

Use `scripts/benchmark-paired.ps1` for at least three fresh baseline/candidate
process pairs in alternating order. Supply matching `-BaselineBench` and
`-CandidateBench` binaries as well as daemon builds: shared queries execute in
the client. Record the build profile, corpus/workload identities, workers,
refinement settings, power context and any affinity masks. Collect an independent
control using identical baseline binaries on both sides.

```powershell
python -B scripts/benchmark-evidence.py --control .scratch/benchmark/control-aa --experiment .scratch/benchmark/candidate-ab --target-case path-broad
```

Declare target cases and thresholds before collecting candidate results. The
engineering gate requires repeated target improvement of at least 10%, 0.1 ms
and 1.25 times observed control noise, with sustained per-cell regression checks.
It preserves individual cases and routes. This is not a significance test or a
guarantee about future noise. A final 4,428-answer same-binary control still
showed substantial broad-query tail variation despite processor affinity.
Small changes need further evidence; passing answer validation alone establishes
neither a speedup nor acceptable latency.

Prepared single/multi-volume and empty/nonempty-delta experiments validated
11,808 timed answers; another 5,535 diagnostic samples exercised refinement
reuse, rescan and bypass. These verify harness coverage rather than quantify an
optimization. No external-product comparison or physical-disk attribution is
claimed by the current measurements.
