# Changelog

## [0.2.0] - 2026-09-09

### Added
- **Grouped terminal output** (#5): one path heading per file with hits,
  `line:text` beneath, blank line between files; pipes keep the flat
  `path:line:text` form byte for byte.
- **`-f` / `--flatten`** (#6): force the flat form on a terminal.
- **Colour** (#7): `--color` / `--no-color`, `NO_COLOR` honoured, auto on a
  terminal. Hand-written SGR (magenta path, green line number, bold red for
  *every* match span on a line via `find_iter`). No new dependency.

### Changed
- **Chunked commits** (#3): the build commits every 4,000 files, 256 MiB of
  pending postings or 1,000,000 distinct pending trigrams
  (`TRIGREP_CHUNK_FILES` / `_BYTES` / `_TRIGRAMS`), one transaction each;
  posting lists are looked up, merged and written one at a time instead of
  buffering every merged blob for the chunk. An in-progress `meta` marker
  makes a build killed between chunks resume before any search answers
  from it (crash test forces 100-file chunks).
- **Binary detection is tgrep's two-part rule** (#3, found while measuring):
  a NUL in the first 8 KiB *or* more than 5% control bytes there, plus an
  extension list (pdf, images, archives, fonts, media, objects, office,
  ML tensors). The NUL rule alone indexed every PDF as text; on a
  9,016-file tree (19 MB of PDFs) that was 9.3M distinct trigrams, a
  173 MB cache, and a build of 59.9 s at 1.65 GB peak RSS. With the rule:
  392k trigrams, a 20 MB cache, **2.3 s at 237 MB** (chunked; 429 MB
  single-commit) — the binaries, not the commit shape, were the memory.
- **Parallel reading and hashing** (#4): `TRIGREP_THREADS` (default
  `available_parallelism` capped at 8). Ids stay in walk order, so the cache
  is byte-identical across thread counts (tested). On the same tree the
  win is now small (2.9 s → 2.3 s); the ticket's 165 s of I/O wait was on
  a much larger disk set.
- `unique_trigrams` uses a 2 MiB bitmap above 256 KiB of input instead of
  collecting one `i64` per byte before dedup.

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
