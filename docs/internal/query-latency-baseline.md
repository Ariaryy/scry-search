# Query latency baseline

> Historical note: this capture predates the truthful span rename. Its
> `match_ns` column corresponds to today's fused `select_ns`, and `rank_ns`
> corresponds to today's delta-merge-and-drain `finalize_ns`.

Reference numbers for the query instrumentation and bounded-materialization
work on a single dev machine. Commit `57d2486` is the pre-instrumentation
reference point; the span capture itself was taken later, after instrumentation
and bounded materialization had landed, so it is not a pre-017 timing capture.
These are diagnostic snapshots from synthetic corpora, not a regression gate — re-run
the underlying tests/benchmarks rather than trusting these numbers to still
hold on a different machine or corpus.

## Span breakdown over a synthetic corpus

`cargo test -p scry-daemon --release -- --ignored span_report --nocapture`
drives the daemon's real query path (cache, span accumulation, memory
sampling) over a small synthetic corpus (6,912 records) built from a fixed
vocabulary, so directory and term structure looks plausible without reading
a real volume.

Keystroke-refinement sequence for `"ledger"` (first query cold, each
subsequent keystroke narrows the same connection's refinement cache):

| step | hits | match_ns | rank_ns | materialize_ns | candidates | emitted |
|---|---|---|---|---|---|---|
| `l` (first) | 50 | 843,100 | 413,500 | 31,000 | 2,690 | 50 |
| `le` | 50 | 687,500 | 126,600 | 28,100 | 576 | 50 |
| `led` | 50 | 0 | 0 | 25,100 | 0 | 50 |
| `ledg` | 50 | 0 | 0 | 24,300 | 0 | 50 |
| `ledge` | 50 | 0 | 0 | 24,300 | 0 | 50 |
| `ledger` | 50 | 0 | 0 | 24,200 | 0 | 50 |

Once a query narrows past the point where the cached candidate set already
satisfies it, `match_ns`/`rank_ns`/`candidates` drop to zero — the refinement
is answered entirely out of the cache with no rescan. `materialize_ns` stays
small and flat (22–33 µs) across every step, including the two cold queries
below: it is not the dominant cost once materialization happens after
ranking and truncation, only for the emitted `limit` rather than the full
overscanned candidate set.

Cold queries (fresh connection, empty cache):

| query | kind | hits | match_ns | rank_ns | materialize_ns | candidates | emitted |
|---|---|---|---|---|---|---|---|
| `.pdf` | Substring | 50 | 372,500 | 187,500 | 37,700 | 1,152 | 50 |
| `TEAMDIR ledger` | PathTerms | 50 | 376,900 | 0 | 25,100 | 50 | 50 |

## Memory

Process memory (`GetProcessMemoryInfo`) sampled cold (before any query) and
warm (after the keystroke sequence and both cold queries above), same
process:

| | private_usage | working_set | peak_working_set | page_faults |
|---|---|---|---|---|
| cold | 21,213,184 | 8,097,792 | 8,097,792 | 2,041 |
| warm | 21,331,968 | 8,716,288 | 8,716,288 | 2,272 |

The 6,912-record corpus and its refinement caches add well under 1 MiB of
private usage across the whole sequence.

## Substring selectivity distribution

`cargo test -p scry-core --release -- --ignored selectivity_distribution --nocapture`
samples 2,000 records from a 500,000-record synthetic corpus (flat, four
cycling name templates — not sampled from a live directory tree) and scores
every distinct prefix length 1–5 plus a set of hardcoded realistic
substrings (`.pdf`, `readme`, `node_modules`, ...) for match-count
selectivity and trigram block survival.

Verdict printed by the test: median p50 selectivity across 3+ byte prefixes
was **3.52%**, below the 5% threshold the test treats as "a rank-ordered
scan over 3+ byte needles looks worth building." Treat this as a directional
signal from a synthetic corpus, not a decision by itself — a flat name
distribution has no directory clustering, which a real volume's substring
queries would exhibit.

### Live-volume selectivity gate

The maintained ignored harness (`SCRY_LIVE_SNAPSHOT=... cargo test -p
scry-core --release live_selectivity_distribution -- --ignored --nocapture`)
sampled 2,000 records from a 1,994,839-record snapshot. It deduplicated their
1–5 byte prefixes, counted every substring match in one multi-pattern archive
pass, and printed aggregates only—never sampled names.

| Prefix bytes | distinct | selectivity p10 | p50 | p90 | block survival p50 | p90 |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 42 | 3.77% | 25.53% | 55.97% | 100.00% | 100.00% |
| 2 | 490 | 0.14% | 1.74% | 7.24% | 100.00% | 100.00% |
| 3 | 953 | 0.01% | 0.10% | 0.90% | 27.19% | 55.82% |
| 4 | 1,148 | 0.00% | 0.01% | 0.30% | 14.67% | 36.69% |
| 5 | 1,245 | 0.00% | 0.00% | 0.17% | 6.98% | 25.19% |

Median selectivity across all sampled 3–5 byte needles was **0.02%**, far
below M2's 5% threshold. M2 therefore passes. This does **not** authorize plan
023 as written: its static rank permutations accelerate Recent/Largest, while
the default Relevance key depends on the current query and must stay on the
filtered scan. Under the harsh storage/compaction budget, the XL design remains
a no-go unless rewritten around default relevance or supported by workload
evidence that non-default orderings dominate.

## Benchmark baseline (`cargo bench -p scry-core`)

Corpus: 440,001 records, 40,001 directories, mean depth 5.8, max depth 7.

| benchmark | time (median) |
|---|---|
| `path_terms/rare` | 6.95 ms |
| `path_terms/clustered` | 43.48 ms |
| `path_terms/mid` | 17.83 ms |
| `path_terms/common` | 68.57 ms |
| `path_terms/rare+common` | 25.69 ms |
| `path_terms/mid+clustered` | 21.89 ms |
| `prefix/1` | 11.97 ms |
| `prefix/3` | 12.64 ms |
| `prefix/8` | 2.60 ms |
| `substring/high` | 54.94 ms |
| `substring/medium` | 11.07 ms |
| `substring/low` | 1.97 ms |
| `wildcard/pdf` | 53.13 ms |
| `wildcard/report` | 12.51 ms |
| `order/Relevance` | 10.62 ms |
| `order/Recent` | 3.39 ms |
| `order/Largest` | 3.78 ms |
| `fixed/path_index_build` | 4.70 ms |
| `fixed/dfs_build` | 24.14 ms |
| `fixed/dfs_size_prefix_build` | 2.06 ms |
| `fixed/closure_all_dirs` | 903 µs |
| `materialize/50` | 174 µs |
| `materialize/1000` | 3.53 ms |
| `materialize/20000` | 78.60 ms |

`materialize/N` scales roughly linearly with `N` (174 µs at 50 to 78.6 ms at
20,000, close to 400x for a 400x increase in count), consistent with each
materialized entry costing one bounded parent-chain walk regardless of how
many candidates were scanned to find it.

### Post-017/018 acceptance rerun

The maintained Criterion suite was rerun after all later plans, on the same
440,001-record deterministic corpus:

| benchmark | recorded baseline | current median | speedup |
|---|---:|---:|---:|
| `materialize/50` | 174 µs | 124.82 µs | 1.39× |
| `materialize/1000` | 3.53 ms | 2.698 ms | 1.31× |
| `materialize/20000` | 78.60 ms | 58.235 ms | 1.35× |
| `substring/high` | 54.94 ms | 14.812 ms | 3.71× |
| `substring/medium` | 11.07 ms | 3.274 ms | 3.38× |
| `substring/low` | 1.97 ms | 1.258 ms | 1.57× |

Plan 018's streaming/top-k substring work is a large, meaningful win, but its
separate **3× materialization target was missed**. At the normal result limit,
the remaining materialization cost is about 125 µs for 50 paths; further work
there is not justified by the harsh compute budget without a new profile.

The plan's real `scry --verbose a <250 ms` gate also remains missed. Ten queries
against the running two-volume daemon measured 1.19–1.25 s warm (1.55 s first
run). Plain CLI input is a PathTerms query, and a one-byte, extremely common
term is the interval algorithm's pathological full-scan/double-decode case—not
the now-fast streaming substring path. Fixing it requires a new decision about
single-term CLI semantics or parallel interval construction; it is not a safe
extension of plan 018.

Plan 017 has an attributable direct comparison. The same release-mode
30,000-record test timed only the second keystroke that overscans 20,000
candidates: the pre-017 parent took 12.753 ms p50 / 19.597 ms p99; current code
took 4.625 ms / 5.198 ms. The **2.76× p50 and 3.77× p99 improvements** clear its
2× gate, while the existing counter test proves no more than the final limit is
materialized.

## Materialization call count

The span report above shows the first `"l"` query scanning 2,690 candidates
but emitting (and therefore materializing) only 50 — a ~54x reduction in
`materialize_one` calls versus materializing every cached candidate before
ranking. `refined_query_materializes_at_most_limit_entries` in
`scry-daemon/src/main.rs` pins this behavior: `spans.emitted` never exceeds
the requested limit even when the refinement cache holds tens of thousands
of candidates.
