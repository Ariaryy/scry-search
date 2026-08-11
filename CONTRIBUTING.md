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

By submitting a contribution, you agree that it may be distributed under the
repository's MIT License.

