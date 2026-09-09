# Changelog

## [0.5.1] - 2026-09-09

### Added

- `--help`/`-h` now exit 0 with usage on stdout (was exit 2 on stderr, indistinguishable from misuse); `--version`/`-V`. `make smoke` (in `make ci`) builds `tg` and runs both; `tests/cli.rs` pins the contract.

## [0.4.0] - 2026-09-09

Three design decisions from #15, confirmed before implementation.

### Fixed
- The cache's stored root is now checked against the root it is opened
  for; a mismatch (a copied cache directory, in principle a hash
  collision) triggers a rebuild instead of silently serving one tree's
  index for another's search.
- An unreadable subdirectory or entry is skipped and counted ("N
  unreadable skipped" in the index report) instead of aborting the
  whole run, matching `grep -r`. The root itself failing to open is
  still a hard error.
- Two `tg` processes racing a root's first-ever index no longer corrupt
  the cache or fail spuriously: the bootstrap's page-1 write is guarded
  by an exclusive lock, and the CLI retries (bounded, exponential
  backoff) on "database is locked" and the schema race a losing
  bootstrap attempt can still hit.

### Changed
- `walk::list_files` returns `(Vec<Entry>, usize)` (files, skip count);
  `index::Stats` gains `skipped_unreadable`.

Test count: 87 -> 90. MC/DC: 13/13 multi-leaf obligations discharged
(one new: the retry loop's guard). Coverage 93.84% (floor 85%).


## [0.3.0] - 2026-09-09

Gate suite adopted from db-core/mvl-lang's approach, plus three real bugs
found while auditing for testability.

### Fixed
- Patterns matching the empty string (`^`, `x*`) reported a phantom
  trailing line on any file ending in `\n` (#11).
- `--` did not escape subcommand dispatch: `tg -- index .` ran the
  indexer instead of searching for the literal word "index" (#12).
- The posting-list varint decoder silently dropped bits 1-6 of a
  malformed 10th byte instead of returning `Overlong` (#13).
- A zero-byte cache file was refused (exit 2); SQLite's own rule treats
  an empty file as a valid empty database, so it is now bootstrapped.
- `mtime == 0` (the "mtime unavailable" fallback) no longer short-circuits
  the content hash — closes a window where an edit could stay invisible.
- `.cargo/config.toml` (a local-dev-only sibling-checkout override) was
  accidentally committed, breaking CI for anyone without that sibling.
  Untracked and gitignored.

### Added
- `deny.toml`, a production panic-lint policy (`[lints.clippy]`:
  `unwrap_used`/`expect_used`/`indexing_slicing`/`panic`/
  `arithmetic_side_effects`/`string_slice`/`cast_*` all deny in `src/`,
  scoped off test code via `clippy.toml` + `lib.rs`), the
  `cargo-mvl-limit` qualified-subset gate (4 files exempted with a
  documented reason), and a CI workflow (lint, deny, mvl-limit, test).
- MC/DC: all 12 multi-condition decisions found by `cargo-mvl-mcdc`
  discharged with tagged test vectors (`make test-mcdc`: PASS).
- Property tests: posting-list encode/decode round-trip, decode never
  panics on arbitrary bytes, `unique_trigrams`' two strategies agree
  across the 256 KiB threshold, and — the correctness property the whole
  index rests on — a pattern's required trigrams are always present in
  any text the regex actually matches.
- CLI tests: multi-window builds byte-identical across thread counts
  1/2/3/7/8, text-to-binary-to-text re-index, a closed pipe (`| head`)
  exits 0 silently, foreign/zero-byte/garbage cache files.
- Test count: 44 -> 84 (plus the crash-torture test). Line coverage
  93.98% (floor 85%).

Companion: t-rust-db/db-storage v0.6.4 adds the `license` field its
`Cargo.toml` was missing (t-rust-db/db-storage#33), pinned here.


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
