<div align="center">

<img src="assets/scry.svg" alt="Scry Search icon" width="112" height="112">

# Scry Search

### File search for Windows, from your terminal or your app

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

Scry Search (`scry` for short) indexes files and folders on fixed NTFS drives
and keeps up with changes as they happen. Search from the command line, or use
the Rust client to add file search to your own application. Scry handles the
index, saved state, ranking and update tracking.

> Use the local compatibility handshake before connecting an application. Pin
> tested client dependencies and use verified releases for daemon updates.

## Features

- Index large collections: about 930,000 files and folders in 10.8 seconds in
  the reference test.
- Leave it running in the background: 55 MiB of private memory and 14 MiB
  resident in RAM after ten minutes idle, with no measurable CPU time.
- Search names and folder paths, with prefix, substring, wildcard and regex
  matching. Filter by file type, extension, size or age, and choose the result order.
- Find new and renamed files in about half a second (median in the reference
  test), without rebuilding the whole index for each change.
- Search as you type. A persistent `SearchSession` cancels outdated requests and
  lets your UI check for results without blocking.
- Reuse one local index from the CLI or an application. The Rust client supports
  both daemon queries and read-only shared search with automatic fallback.

See [performance](#performance) for the hardware, workload and measurement limits.

## Install

[Download the latest release](https://github.com/Ariaryy/scry-search/releases),
extract the archive, open PowerShell inside the extracted folder, then run:

```powershell
powershell -ExecutionPolicy Bypass -File .\install-daemon.ps1
```

Releases are not code-signed yet, so Windows SmartScreen may warn about the
downloaded archive or executables. Verify that the download came from this
repository's Releases page before choosing **Run anyway**.

The installer asks for administrator access once and starts `scryd.exe`, the
background indexer. It also sets Scry to start when you sign in. A scheduled
task runs it with the required permissions under your Windows account; Scry
does not install a system service.

The installer copies the binaries to
`%ProgramFiles%\Scry Search\<user SID>\bin` and adds that directory to the user
`PATH`, so `scry` is available in new terminals. The extracted archive can then
be deleted.

<details>
<summary>Index storage, upgrades and removal</summary>

Per-volume snapshots and their FRN sidecars are stored separately under
`%LOCALAPPDATA%\scry` as `index-<volume>.rkyv` and
`index-<volume>.frn`. Uninstalling the binaries preserves this index data so a
later reinstall does not require rebuilding it unnecessarily.

Use `scry update` for updates. See the [updater contract and limitations](docs/users/updating.md).

Run `uninstall.ps1` in the protected installation directory to remove the task
and programs while preserving snapshots.

</details>

## Search

```powershell
scry notes
scry 'projects "annual report" type:file ext:pdf,docx'
scry 'type:dir modified:<7d'
scry '*.toml'
scry --sort recent notes
scry --interactive
```

Search terms ignore case and can match a filename or any folder in its path.
See the [search syntax](docs/users/search-syntax.md) for matching and filters,
the [CLI reference](docs/users/cli.md) for command options, and the
[daemon guide](docs/users/daemon.md) for indexer settings.

## Performance

Tests on a Windows PC with an Intel Core i5-13500HX and 16 GiB RAM
produced these results. Query timings use a synthetic 440,001-record index;
indexing, update and idle measurements use about 930,000 files and folders:

| Task | Measured result |
|---|---:|
| Build the initial index | 10.8 seconds |
| Search by the first few letters of a name | 1.1 ms median |
| Search for text within a name | 1.9 ms median |
| Broad single-letter search | 48 ms median |
| Make a new or renamed file searchable | About half a second median |
| Memory after ten minutes idle | 55 MiB private memory; 14 MiB resident in RAM |

During the ten-minute idle test, Scry recorded no measurable CPU time and no
file read/write activity in its process counters. Windows gradually reclaimed
resident memory while Scry's private memory stayed around 55 MiB.

Search times use release builds and shared-index execution, with 60 warm
samples per query returning up to 50 results. The indexing, update and idle
figures are from the earlier lifecycle test. Broad searches take longer because
they match more of the index. Results vary with
hardware, filenames, folder structure and recent activity; the initial indexing
test did not clear Windows' disk cache.

See the [benchmark guide](docs/internal/benchmarks.md) for the full results,
test setup and instructions to reproduce them.

## Embed it

Use the Rust client to keep searches off your UI thread. Connect once, submit
queries as the user types, and poll for the latest results:

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

## Built with Scry Search

- [Hayai](https://github.com/Ariaryy/hayai) uses the Rust client and elevated
  daemon to provide realtime file search inside a Windows application launcher.

## Build from source

```powershell
git clone https://github.com/Ariaryy/scry-search.git
cd scry-search

cargo build --profile daemon-release -p scry-daemon
cargo build --release -p scry-cli
```

`scryd` needs elevation for complete raw NTFS indexing. `scry` and applications
using the Rust client stay unelevated.

Initial indexing reads volume metadata and temporarily uses more memory.
By default, the daemon limits indexing reads to 128 MiB/s so other applications
can still use the disk. Use `scryd --index-mbps N` to change the limit or
`scryd --unbounded` to remove it.

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

The daemon reads NTFS metadata, saves a compact memory-mapped index, and follows
the USN change journal. Recent changes are kept alongside the saved index and
published together so searches see a consistent view of each volume.

Searches run across volumes and build paths only for the results returned.
Broad queries retain a limited set of ranked candidates instead of allocating
memory for every match. Index compaction streams through files to keep private
memory bounded. Windows can reclaim unused mapped pages independently.

Idle CPU and disk activity should approach zero. Performance work must preserve
those limits as well as improve search speed.

Read [architecture](docs/internal/architecture.md), [design decisions](docs/internal/design-decisions.md),
and the [v10 index format](docs/internal/index-format.md) before changing these
parts of the implementation.

## Platform and limitations

- Windows 10/11; complete indexing requires fixed NTFS volumes and elevation.
- When raw filesystem metadata is unavailable, fallback indexing may leave some
  file sizes unknown. Scry keeps those distinct from empty files.
- Client and daemon versions must match.
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
