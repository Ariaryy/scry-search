# Changelog

## 0.1.1 - 2026-10-22

- Reconcile daemon pipe ownership when a process exits or is replaced during an update.
- Retain process handles through graceful shutdown and readiness verification.
- Keep ownership and access checks fail closed without force-stopping unknown processes.

IPC compatibility remains version 1 and index format remains v10. Applications
must bundle the refreshed CLI to receive this fix because the updater is embedded
in that executable. The full Hayai downgrade/update acceptance test remains pending;
its previous attempt was blocked by GitHub API rate limiting before daemon shutdown.

## 0.1.0 - 2026-10-22

- Add local daemon compatibility discovery independently of application versions.
- Add explicit compatible-release discovery and verified exact-target updates.
- Keep update progress in the originating terminal through a hidden UAC helper.
- Protect elevated task executables in Program Files and recover interrupted activation.
- Preserve index format v10 and IPC compatibility version 1.

Windows x86_64 releases are unsigned. Existing packages without release manifests
are unsupported by the updater. Running legacy daemons must be stopped before
migration. Index-format-changing updates are refused rather than rolled back.

## 0.1.0-alpha.4 — 2026-10-21

- Add selective path-term query planning for narrow combinations.
- Avoid whole-index bitmaps for sparse path matches, cap retained query scratch,
  and share dense path membership across workers.
- Reuse matching and name-derived ranking for identical consecutive names.
- Improve case-insensitive substring candidate scans and bucket decoding.
- Fix case-insensitive ranking during daemon refinement and bound cancellation
  checks during dense bitmap traversal, including empty regions.
- Add validated RPC/shared benchmark tooling, lifecycle measurements and paired
  synthetic comparisons. Results include workload-specific regressions; see
  [the full comparison](docs/internal/release-query-comparison.md).
- Separate core, daemon, client and filesystem responsibilities into private
  modules while preserving public import paths.
- Add application icons and improve installation, usage and performance docs.

Snapshot format remains v10. Install matching daemon and client builds.
Complete indexing targets Windows 10/11 fixed NTFS volumes with an elevated
daemon. This is an unsigned alpha prerelease.

[Changes since alpha.3](https://github.com/Ariaryy/scry-search/compare/v0.1.0-alpha.3...v0.1.0-alpha.4)

## Earlier releases

See [GitHub Releases](https://github.com/Ariaryy/scry-search/releases) for the
alpha.1–alpha.3 notes and downloads.
