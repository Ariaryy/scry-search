<div align="center">

# Scry Search

### A tiny, realtime file index for Windows

Fast path search · live NTFS updates · embeddable Rust client · local-only IPC

</div>

Scry Search (`scry` for short) indexes fixed NTFS volumes from filesystem metadata, keeps a compact
memory-mapped snapshot, follows the USN journal, and serves searches to CLIs or
desktop applications. It is designed for software that needs a file-search
backend without embedding filesystem crawling, persistence, ranking, and update
tracking itself.

Scry is intentionally engineered under a **harsh computing budget**. Idle CPU
and disk activity should approach zero; memory growth must be bounded; broad
queries must not allocate per match; and a speedup that quietly increases
background work is usually a regression. Performance is part of correctness.

> **Status:** `0.1.0-alpha.1`. The on-disk and wire formats may change
> between versions; package matching client and daemon builds together.

## Built with Scry Search

- [Hayai](https://github.com/Ariaryy/hayai) uses the Rust client and elevated
  daemon to provide realtime file search inside a Windows application launcher.

## Why Scry?

- **Realtime:** a persistent `SearchSession` cancels superseded keystrokes and
  lets UI code poll without blocking.
- **Small at rest:** immutable columns are memory-mapped; cold metadata can be
  paged out independently.
- **Bounded work:** matching retains top-k integer keys and reconstructs paths
  only for emitted results.
- **Live updates:** structural journal changes enter a bounded delta instead of
  rebuilding the full index.
- **Useful queries:** ancestor-aware path terms, prefix, substring, wildcard,
  regex, file/directory type, extension, size, age, and ranking controls.
- **Two client paths:** ordinary named-pipe RPC plus validated read-only local
  execution with automatic RPC fallback.

## Measured behavior

**Measurement set:** recorded for Scry Search `0.1.0-alpha.1`.

These are diagnostic measurements from one Windows development machine, not
universal promises. Corpus shape, storage, CPU, cache warmth, query, and result
limit all matter. No numbers below compare Scry with another product.

| Scenario | Corpus | Observed result |
|---|---:|---:|
| Steady-connection absent query | ~2.7M records, two volumes, 200 samples | 0.020 ms p50 / 0.334 ms p99 RPC round trip |
| Selective two-term query, fresh process/connection | ~2.7M records, 200 samples | 21.449 ms p50 / 24.197 ms p99 |
| Pathological one-byte path term | ~2.7M records, 10 warm samples | 125.33 ms p50 / 246.19 ms p99 |
| Streaming compaction | 224,231-record base → 236,232-record snapshot | 0.846 s; 29.23 MB peak private commit |
| Ten-minute idle run after compaction | 236,232 records, 1 Hz sampling | 18.43–18.50 MB private commit; 1.95–3.04 MB working set; zero observed I/O/reindexes/compactions |
| Idle Task Manager snapshot | current daemon, mapped pages mostly cold | ~3 MB private / ~7 MB total / ~4 MB shared working set |

The maintained synthetic and live-snapshot probes are ignored tests so normal
CI stays deterministic. Reproduce the portable suite with:

```powershell
cargo test --workspace
cargo bench -p scry-core --bench query
```

Live-volume measurements require an elevated shell and explicitly supplied
snapshot or volume; see [internal performance notes](docs/internal/query-latency-baseline.md).

## Install

Download and extract the Windows archive, then run:

```powershell
powershell -ExecutionPolicy Bypass -File .\install-daemon.ps1
```

Windows shows one UAC prompt. The script registers `scryd.exe` as a
highest-privilege **per-user scheduled task** at logon and starts it. Scry uses
this instead of a system service because its snapshot, clients, and IPC endpoint
are user-scoped. It installs the binaries under
`%LOCALAPPDATA%\Programs\Scry Search\bin` and adds that directory to the user
`PATH`, so `scry` is available in new terminals. The extracted archive can then
be deleted.

To remove the startup task while preserving snapshots:

```powershell
powershell -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\Scry Search\uninstall.ps1"
```

### Build from source

```powershell
cargo build --profile daemon-release -p scry-daemon
cargo build --release -p scry-cli
```

`scryd` needs elevation for complete raw NTFS indexing. `scry` and applications
using the client SDK stay unelevated.

Initial indexing is the exceptional heavy phase: it reads volume metadata and
can temporarily raise memory usage as pages are built and touched. By default,
the daemon caps aggregate indexing reads at **128 MiB/s** to protect foreground
disk latency. Use `scryd --index-mbps N` for a custom cap or `scryd --unbounded`
when finishing as quickly as possible matters more than competing I/O.

## Search

```powershell
scry report
scry 'projects "annual report" type:file ext:pdf,docx'
scry 'type:dir modified:<7d'
scry '*.toml'
scry --sort recent notes
scry --interactive
```

Bare words are case-insensitive path terms: each may match the leaf or an
ancestor directory. See the full [search syntax](docs/users/search-syntax.md).
All daemon and CLI switches are listed in the [daemon guide](docs/users/daemon.md)
and [CLI reference](docs/users/cli.md).

## Embed it

```rust,no_run
use scry_client::{QueryKind, SearchSession};

let mut search = SearchSession::connect(50)?;
search.submit(QueryKind::PathTerms, "report type:file ext:pdf")?;

// Call from an event loop or short timer; this never waits for the query.
if let Some(results) = search.poll_latest()? {
    for result in results {
        println!("{}", result.path);
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

See the [Rust SDK guide](docs/users/rust-client.md), [IPC guide](docs/users/ipc.md),
and runnable [`examples/`](examples/). A stable C ABI is not available yet.

## Architecture

```text
NTFS MFT + USN journal
          │
        scryd ── snapshot + FRN sidecar
          │
    named-pipe RPC / read-only shared section
          │
      scry-client
       ╱       ╲
     CLI      applications
```

The daemon publishes one coherent immutable-base-plus-delta view per volume.
Queries fan out across volumes, retain bounded top-k hits, merge rank keys, and
materialize only returned paths. Compaction streams through file-backed columns
to avoid an index-sized private-memory spike.

Read [architecture](docs/internal/architecture.md), [design decisions](docs/internal/design-decisions.md),
and the [v10 index format](docs/internal/index-format.md) before changing these
boundaries.

## Platform and limitations

- Windows 10/11; complete indexing requires fixed NTFS volumes and elevation.
- The fallback enumerator cannot recover every size available to the raw MFT
  reader; unknown sizes remain explicitly marked unknown.
- Snapshot and IPC compatibility are currently release-coupled.
- The Rust SDK is the supported embedding API today.

## Contributing

Issues and focused pull requests are welcome. Run formatting, clippy, and the
workspace tests before submitting:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

Scry is available under the [MIT License](LICENSE).
