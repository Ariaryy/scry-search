# Contributing

Thank you for improving Scry. Before opening a pull request:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Keep changes focused and explain correctness or performance tradeoffs. New
benchmarks must use generic generated vocabulary, state the corpus and machine
conditions, and avoid presenting local measurements as universal results.

Filesystem records and IPC payloads are hostile input. Preserve checked slicing,
bounded loops, cancellation, result-limit clamps, and the canonical-tree rules
documented in [`docs/internal/`](docs/internal/).

## Resource budget

Scry treats compute usage as a first-class acceptance gate:

- no periodic disk writes or meaningful CPU work while idle;
- no index-sized private allocation during build or compaction when a mapped or
  streaming representation is possible;
- no candidate-sized allocation for bounded top-k queries;
- no per-record path reconstruction, case folding, or heap allocation before a
  candidate survives selection;
- no performance claim without before/after numbers, corpus size, build mode,
  query shape, and enough methodology to rerun it.

An optimization that improves latency by materially increasing idle work,
memory, write amplification, or worst-case behavior needs an explicit design
decision and evidence that the trade is worthwhile.

By submitting a contribution, you agree that it may be distributed under the
repository's MIT License.
