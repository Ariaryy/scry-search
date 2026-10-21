# Release-to-planner query comparison

Matched `release` source builds compare `v0.1.0-alpha.3` (`45f54cc`) with
planner candidate `fac0bd9` (merged through #21). These measurements precede the
#22 module refactor and alpha.4 packaging; they are not timings of the final ZIP.
Rust 1.99/MSVC, Windows x64, Intel Core i5-13500HX, High performance plan,
affinity mask 85; opt-level 3, LTO, one codegen unit, unwind. Shared client uses
the Windows System allocator; daemon uses mimalloc 0.1.52. Power source,
thermals, OS cache and other activity were uncontrolled.

Alpha.3 predates the harness: its private adapter adds frozen startup and
benchmark diagnostics, leaving its search core unchanged. Both runners use the
same harness. The shared-index API includes parsing, matching, ranking and up
to 50 constructed paths. Every accepted sample used shared search without RPC
fallback. Local search is single-threaded; the daemon's four workers do not
execute these queries. One active query, empty delta, Relevance order except
`recent_path`, which orders by modification time.

Four synthetic mapped corpora: standard440k (440,001 unique-name records),
standard2700k (2,700,001 unique-name records), calibrated2700k (2,700,000
records with repeated names), sparse2700k (2,700,002 records with one
1,026-position matching subtree). Hashing and reference searches precede
timing; these are warm tests, not cold-cache results.

Three independent same-baseline A/A pairs and three alternating A/B process
pairs per corpus; two warmups and 20 primary samples per query/process give
60 A/B warm samples per build/cell. There were 8,160 validated primary warm
answers and 408 separately timed first answers, with zero ordered-answer
mismatches. Validation runs outside timers. The table pools A/B warm samples;
p95 uses nearest rank. The first answers are excluded.

`path_p` and `substring_p` use a single-letter term in the path or own name.
`path_report` and `substring_report` use a six-letter term. `common_path` uses
a common ancestor term. `rare_common` and `rare_short` combine a rare term with
a common or single-letter term; standard corpora return no matches, calibrated
corpora use a rare filename key. `prefix` matches a synthetic name prefix;
`wildcard` matches a synthetic extension; `negative` is an absent term;
`sparse_ancestor` matches the small subtree. Returned results are capped at 50.

| Corpus / query | Alpha.3 p50 ms | Candidate p50 ms | Alpha.3 p95 ms | Candidate p95 ms | Benefit screen | Regression screen |
|---|---:|---:|---:|---:|---|---|
| standard440k / path_p | 157.2262 | 48.3170 | 169.4106 | 51.5444 | yes | no |
| standard440k / substring_p | 21.7798 | 21.1763 | 24.5506 | 22.4186 | no | no |
| standard440k / path_report | 10.5695 | 4.4559 | 11.8325 | 4.8539 | yes | no |
| standard440k / substring_report | 1.9262 | 1.9259 | 2.1672 | 2.1816 | no | yes |
| standard440k / common_path | 69.9185 | 28.2041 | 73.0034 | 29.5279 | yes | no |
| standard440k / rare_common | 12.7128 | 1.2851 | 13.3520 | 1.5847 | yes | no |
| standard440k / rare_short | 23.4829 | 1.5667 | 26.3036 | 1.8888 | yes | no |
| standard440k / prefix | 1.1209 | 1.0886 | 1.3315 | 1.2135 | no | no |
| standard440k / wildcard | 26.0216 | 27.6768 | 27.2272 | 28.9739 | no | yes |
| standard440k / recent_path | 33.2037 | 27.0694 | 34.8431 | 29.1801 | yes | no |
| standard440k / negative | 0.0304 | 0.0295 | 0.0367 | 0.0348 | no | no |
| standard2700k / path_p | 1266.5432 | 310.5490 | 1588.0300 | 317.7953 | yes | no |
| standard2700k / substring_p | 125.4155 | 128.7803 | 135.4323 | 131.9633 | no | no |
| standard2700k / path_report | 65.9944 | 26.9944 | 78.7553 | 28.1129 | yes | no |
| standard2700k / substring_report | 8.7162 | 9.1565 | 10.1583 | 10.0268 | no | yes |
| standard2700k / common_path | 528.7892 | 173.7728 | 619.6934 | 180.4098 | yes | no |
| standard2700k / rare_common | 75.4026 | 84.6287 | 79.9986 | 89.0255 | no | yes |
| standard2700k / rare_short | 143.5765 | 92.6990 | 156.9690 | 96.4234 | yes | no |
| standard2700k / prefix | 6.5713 | 6.5766 | 7.3993 | 7.2274 | no | no |
| standard2700k / wildcard | 152.2290 | 168.2529 | 166.6690 | 175.5776 | no | yes |
| standard2700k / recent_path | 249.5725 | 224.6948 | 268.3700 | 260.1593 | no | no |
| standard2700k / negative | 0.0325 | 0.0317 | 0.0351 | 0.0433 | no | no |
| calibrated2700k / path_p | 600.8916 | 140.5339 | 798.6822 | 159.4282 | yes | no |
| calibrated2700k / substring_p | 79.7150 | 55.5241 | 84.2739 | 58.6168 | yes | no |
| calibrated2700k / path_report | 191.6108 | 79.2300 | 354.0710 | 89.6411 | yes | no |
| calibrated2700k / substring_report | 108.0686 | 56.8517 | 115.9785 | 61.1837 | yes | no |
| calibrated2700k / common_path | 1099.3222 | 108.2024 | 1401.9610 | 113.9074 | yes | no |
| calibrated2700k / rare_common | 36.2335 | 30.2460 | 38.6979 | 32.9738 | yes | no |
| calibrated2700k / rare_short | 182.0694 | 9.8973 | 197.9191 | 11.1001 | yes | no |
| calibrated2700k / prefix | 56.1446 | 56.3501 | 57.6752 | 59.0405 | no | no |
| calibrated2700k / wildcard | 141.8139 | 61.4582 | 164.5583 | 65.4594 | yes | no |
| calibrated2700k / recent_path | 177.9142 | 114.4929 | 231.3948 | 129.8051 | no | no |
| calibrated2700k / negative | 0.0323 | 0.0320 | 0.0411 | 0.0730 | no | no |
| sparse2700k / sparse_ancestor | 0.2678 | 0.1597 | 0.3395 | 0.1935 | yes | no |

A benefit requires p50 savings of at least 10%, 0.1 ms and 1.25 times independent
A/A noise in every pair. A regression requires p50 or p95 cost above 5%, 0.1 ms
and 1.25 times corresponding A/A noise in at least two pairs. These finite
engineering screens are not significance tests or bounds on future noise.
Some cells regress despite the large path-query gains; no universal speedup is
claimed. In particular, standard2700k rare/common terms rise from 75.4026 to
84.6287 ms p50, and wildcard from 152.2290 to 168.2529 ms.

The alpha.3 RPC pilot failed full-entry validation and was excluded. A separate
400-request count-only diagnostic observed one unexpected empty alpha.3 reply
and none on the candidate. This does not establish a general failure rate.
No release-to-candidate RPC latency comparison is claimed. Private raw captures,
source/binary hashes and adapter diff remain under ignored benchmark storage.

See [benchmark methodology](benchmarks.md) for maintained reproduction tools.
