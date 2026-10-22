# Daemon setup

`scryd` must run elevated to read the raw NTFS metadata used for complete,
fast indexing. Start it once and keep it alive; clients should not launch a new
daemon for each query.

The release installer registers the elevated per-user task `Scry Search Daemon`.
For updates and application integration, see [Updating Scry Search](updating.md).
Applications should distinguish missing installations, stopped daemons, incompatible
versions, and transport failures. Clients remain unelevated.

## Daemon options

```text
scryd [OPTIONS] [VOLUME ...]

--index-mbps N      cap aggregate initial-index reads to N MiB/s
--index-mbps=N      equivalent inline form
--unbounded         disable the read cap
-h, --help          print help
-V, --version       print version
```

Without explicit volume names, the daemon indexes every accessible fixed NTFS
volume. The default aggregate read cap is 128 MiB/s; `SCRY_INDEX_MBPS` changes
that default. `--index-mbps` wins over the environment variable, and
`--unbounded` is equivalent to a zero cap.

Initial indexing necessarily reads filesystem metadata and may temporarily
increase working set/private memory. The cap protects foreground disk latency;
`--unbounded` may finish sooner but can saturate storage and is not the polite
default.

The release installer accepts matching task settings:

```powershell
.\install-daemon.ps1                 # default 128 MiB/s cap
.\install-daemon.ps1 -IndexMbps 64   # custom cap
.\install-daemon.ps1 -Unbounded      # maximum indexing throughput
```

The installer copies `scryd.exe` and `scry.exe` to
`%ProgramFiles%\Scry Search\<user SID>\bin`, adds that directory to the user
`PATH`, registers the elevated logon task, and starts the daemon in the
background. Use `-NoStart` only when the daemon should wait until the next
logon or a manual task start. Open a new terminal before invoking `scry` by
name.

## Data location

The daemon stores per-user index data under `%LOCALAPPDATA%\scry`:

- `index-<volume>.rkyv` is the memory-mapped snapshot for one volume.
- `index-<volume>.frn` maps filesystem record numbers for incremental updates.
- Temporary compaction files may appear briefly in a per-volume scratch
  directory beneath the same location. The files are removed when compaction
  ends; the empty scratch directory may remain for reuse.

This directory is separate from the protected program directory, which
contains the installed executables. The uninstaller removes the executables,
PATH entry, and scheduled task but deliberately preserves the index directory.


