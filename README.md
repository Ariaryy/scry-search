<div align="center">

<img src="assets/scry.svg" alt="Scry Search icon" width="112" height="112">

# Scry Search

### A tiny, realtime file index for Windows

Fast path search · live NTFS updates · embeddable Rust client · local-only IPC

<p>
  <a href="https://github.com/Ariaryy/scry-search/releases"><img src="https://img.shields.io/github/v/release/Ariaryy/scry-search?include_prereleases&amp;label=release" alt="Latest release"></a>
  <img src="https://img.shields.io/badge/platform-Windows%2010%2F11-0078D4" alt="Windows 10 and 11">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT license"></a>
</p>

<p>
  <a href="#install">Quick start</a> ·
  <a href="docs/users/search-syntax.md">Query language</a> ·
  <a href="docs/users/cli.md">CLI</a> ·
  <a href="docs/users/rust-client.md">Rust client</a> ·
  <a href="docs/users/ipc.md">IPC</a> ·
  <a href="#performance">Performance</a> ·
  <a href="docs/internal/architecture.md">Architecture</a> ·
  <a href="https://github.com/Ariaryy/scry-search/releases">Releases</a>
</p>

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

> **Status:** `0.1.0-alpha.3`. The on-disk and wire formats may change
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

## Performance

Tests with **about 930,000 files and folders** on a Windows PC with an Intel
Core i5-13500HX and 16 GiB RAM gave the following results:

| Task | Measured result |
|---|---:|
| Build the initial index | 10.8 seconds |
| Search by the first few letters of a name | 0.4 ms median |
| Search for text within a name | 8 ms median |
| Broad single-letter search | 161 ms median |
| Make a new or renamed file searchable | About half a second median |
| Memory after ten minutes idle | 55 MiB private memory; 14 MiB resident in RAM |

During the ten-minute idle test, Scry recorded no measurable CPU time and no
file read/write activity in its process counters. Windows gradually reclaimed
resident memory while Scry's private memory stayed around 55 MiB.

Search times are from repeated searches returning up to 50 results. Broad
searches take longer because they match more of the index. Results vary with
hardware, filenames, folder structure and recent activity; the initial indexing
test did not clear Windows' disk cache.

See the [benchmark guide](docs/internal/benchmarks.md) for the full results,
test setup and instructions to reproduce them.

## Install

[Download the latest release](https://github.com/Ariaryy/scry-search/releases),
extract the archive, open PowerShell inside the extracted folder, then run:

```powershell
powershell -ExecutionPolicy Bypass -File .\install-daemon.ps1
```

Releases are not code-signed yet, so Windows SmartScreen may warn about the
downloaded archive or executables. Verify that the download came from this
repository's Releases page before choosing **Run anyway**.

Windows shows one UAC prompt. The script registers `scryd.exe` as a
highest-privilege **per-user scheduled task** at logon and starts it. Scry uses
this instead of a system service because its snapshot, clients, and IPC endpoint
are user-scoped.

The installer copies the binaries to
`%LOCALAPPDATA%\Programs\Scry Search\bin` and adds that directory to the user
`PATH`, so `scry` is available in new terminals. The extracted archive can then
be deleted.

Per-volume snapshots and their FRN sidecars are stored separately under
`%LOCALAPPDATA%\scry` as `index-<volume>.rkyv` and
`index-<volume>.frn`. Uninstalling the binaries preserves this index data so a
later reinstall does not require rebuilding it unnecessarily.

To upgrade, download the newer release and run its installer again. It replaces
the installed binaries and preserves existing per-volume snapshots.

To remove the startup task while preserving snapshots:

```powershell
powershell -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\Scry Search\uninstall.ps1"
```

### Build from source

```powershell
git clone https://github.com/Ariaryy/scry-search.git
cd scry-search

cargo build --profile daemon-release -p scry-daemon
cargo build --release -p scry-cli
```

`scryd` needs elevation for complete raw NTFS indexing. `scry` and applications
using the Rust client stay unelevated.

Initial indexing is the exceptional heavy phase: it reads volume metadata and
can temporarily raise memory usage as pages are built and touched. By default,
the daemon caps aggregate indexing reads at **128 MiB/s** to protect foreground
disk latency. Use `scryd --index-mbps N` for a custom cap or `scryd --unbounded`
when finishing as quickly as possible matters more than competing I/O.

## Search

```powershell
scry notes
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

See the [Rust client guide](docs/users/rust-client.md), [IPC guide](docs/users/ipc.md),
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
- The Rust client is the supported embedding API today.
- Linux support is planned for a future release.

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
