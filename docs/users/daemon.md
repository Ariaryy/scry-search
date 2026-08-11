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

