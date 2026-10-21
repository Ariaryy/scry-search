# Changelog

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
