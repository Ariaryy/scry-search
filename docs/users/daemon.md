# Daemon setup

`scryd` must run elevated to read the raw NTFS metadata used for complete,
fast indexing. Start it once and keep it alive; clients should not launch a new
daemon for each query.

An installer and automatic elevation flow are not shipped yet. Until they are,
start `scryd` from an elevated terminal. An application embedding Scry should
distinguish these states:

1. daemon reachable — connect and reuse the session;
2. daemon installed but stopped — offer an explicit UAC start action;
3. daemon unavailable — show installation guidance and keep non-file features
   working.

A per-user elevated startup task is the intended default deployment model. A
Windows service is not currently required: Scry's snapshots and IPC endpoint are
user-scoped, while a service would need additional identity, authorization, and
session-lifecycle machinery.

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


