# Changelog

## [0.1.1] - 2026-09-09

- Fix: package metadata said MIT; the repository is Apache-2.0 (as its
  `LICENSE` file already was). No code change.

## [0.1.0] - 2026-09-09

Extracted from `t-rust-db/sqlite-rs` (the `sqlgrep` binary, sqlite-rs
ADR-0043 and spec 013, issues #34 and #38 there) into its own repository
and renamed: crate `trigrep`, binary `tg`, cache under `trigrep/`, env
`TRIGREP_CACHE_DIR`. Behaviour unchanged.

- Depends on `db-storage` directly (`row` feature) instead of sqlite-rs's
  re-exports; the one sqlite-rs-only helper used (`dump::open`) is replaced
  by a local `cache::open_db` without the WAL-fallback path, which a cache
  this tool always writes itself never needs.
- `regex`, `regex-syntax` and `dirs` are ordinary dependencies of a binary
  crate; the feature gate that carried them in sqlite-rs is gone.
- The former per-search freshness `stat` scan is off by default; `-u` /
  `--update` turns it on (sqlite-rs #38).
