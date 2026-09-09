> Carried over from sqlite-rs spec 013 on 2026-09-09; names updated.

---
domain: trigrep
version: 0.1.0
status: draft
date: 2026-09-08
---

# 013 — `trigrep`: Serverless Trigram Grep

A second binary in this crate (#34, ADR-0043): trigram-indexed grep over a
tree, where the index is a real SQLite-format file per root, maintained
through `db-storage`'s b-tree/pager layer with no SQL, no daemon and no
format of its own. Every invocation opens the cache (building it on a
root's first-ever search) and exits; a plain search after that reuses the
cache exactly as it stands, and only `trigrep index` or `-u`/`--update`
(#38) explicitly brings it up to date with the filesystem.

Refs: #34 (Requirements 1-6); #38 (Requirement 7); ADR-0043.

## Tier Position

`trigrep` is an application over Tier 0 (the storage core) the way the
`sqlite-rs` CLI is an application over db-cli; it adds no claim to any
tier, but its cache file must satisfy Tier 0's file-format contract
(Requirement 2) so `sqlite3` can open it.

## Requirements

### Requirement 1: One cache file per canonical root [MUST]

The cache for a root MUST live at `$TRIGREP_CACHE_DIR/<key>.db` or, absent
that variable, under the platform cache directory (`dirs::cache_dir()`)
in a `trigrep/` subdirectory, where `<key>` is derived from the root's
canonicalized path — two spellings of one directory MUST share a cache,
two directories MUST NOT.

**Implementation:** `src/bin/trigrep/cache.rs::cache_path`

#### Scenario: Distinct roots, distinct files; one root, one file

- GIVEN two sibling directories `a` and `b`
- WHEN `trigrep cache-path` is asked for `a`, `b`, and `b/../a/.`
- THEN `a` and `b` map to different files under the cache directory and
  `a` and `b/../a/.` map to the same one

**Tests:** `tests/unit/trigrep_cli_test.rs::one_cache_file_per_canonical_root`

### Requirement 2: The cache is a byte-compatible SQLite database [MUST]

The cache MUST be a valid SQLite-format file: `PRAGMA integrity_check`
reports `ok`, and `sqlite_master` lists tables `files`, `trigrams` and
`meta` with real `CREATE TABLE` text. It MUST be created and written only
through `db-storage`'s b-tree and pager APIs (no SQL is compiled).

**Implementation:** `src/bin/trigrep/cache.rs::open`

#### Scenario: A built cache passes integrity_check with trigrep's schema

- GIVEN a tree of three text files
- WHEN `trigrep index` has run
- THEN opening the cache with this crate's reader and running
  `run_integrity_check` yields `ok`, and the schema names are exactly
  `files`, `meta`, `trigrams`

**Tests:** `tests/unit/trigrep_cli_test.rs::index_then_search_prints_file_line_matches_and_grep_exit_codes`

### Requirement 3: Search prints file:line matches with grep exit codes [MUST]

`trigrep <pattern> [path]` MUST build the cache if absent, then print each
matching line; exit 0 when something matched, 1 when nothing did, 2 on
error (invalid regex included). When stdout is not a terminal, or with
`-f`/`--flatten`, the form MUST be one `path:line:text` line per match
(Requirement 8 covers the terminal form). Narrowing by
required literal trigrams MUST never drop a match the regex would find
(classes, alternations and optional groups contribute no required
trigrams; `-i` scans every file).

**Implementation:** `src/bin/trigrep/search.rs::run`

#### Scenario: Matches across files, no match, bad pattern

- GIVEN files containing `needle_one` on known lines and one without
- WHEN searching for `needle_one`, then `zzz_absent_zzz`, then `(`
- THEN the first prints both `file:line:text` hits and exits 0, the
  second prints nothing and exits 1, the third exits 2

**Tests:** `tests/unit/trigrep_cli_test.rs::index_then_search_prints_file_line_matches_and_grep_exit_codes`

#### Scenario: Cache is built lazily on first search

- GIVEN a root with no cache file yet
- WHEN a search runs
- THEN it finds the match and the cache file exists afterwards

**Tests:** `tests/unit/trigrep_cli_test.rs::search_without_prior_index_builds_the_cache_first`

#### Scenario: Regex narrows by literals but decides by regex

- GIVEN lines `foo123bar`, `foo bar`, `fooXbar`
- WHEN searching `foo[0-9]+bar`, then `o.b` (no 3-byte literal), then
  `-i FOOX`
- THEN exactly the regex-matching lines are printed in each case

**Tests:** `tests/unit/trigrep_cli_test.rs::regex_patterns_narrow_by_literals_but_match_by_regex`

### Requirement 4: Incremental update in bounded chunks [MUST]

`trigrep index` MUST touch only files whose mtime or size changed since the
last index (re-hashing content to confirm), add new files, and stop
returning deleted ones. Writes MUST be committed in windows — every
`TRIGREP_CHUNK_FILES` files (default 4,000), `TRIGREP_CHUNK_BYTES` of
pending postings (default 256 MiB) or `TRIGREP_CHUNK_TRIGRAMS` distinct
pending trigrams (default 1,000,000), whichever comes first — each window
one pager transaction committed by one `flush` and carrying its file rows
and the posting lists they touch. A `meta` marker (rowid 2) MUST be present
from the first window's commit to the last's, and a cache opened with the
marker present MUST be brought up to date before any search answers from
it. An invocation with nothing changed MUST rewrite no posting list. File
reading and hashing MAY run on several threads (`TRIGREP_THREADS`), but
file ids MUST be assigned in walk order so the resulting cache is
byte-identical for any thread count.

**Implementation:** `src/bin/trigrep/index.rs::update`

#### Scenario: Add, edit, delete are each reflected; unchanged is free

- GIVEN an indexed tree of three files
- WHEN one is edited, one deleted and one added, then `index` runs again
- THEN the report says `1 added, 1 changed, 1 removed, 1 unchanged`, the
  new and edited content is found, the deleted and pre-edit content is
  not, and a no-change pass reports `0 posting lists rewritten`

**Tests:** `tests/unit/trigrep_cli_test.rs::incremental_update_touches_only_changed_files`

#### Scenario: `--rebuild` discards the cache

- GIVEN a cache grown by an added-then-deleted file
- WHEN `trigrep index --rebuild` runs
- THEN the cache is byte-for-byte the size of a fresh build and still
  answers queries

**Tests:** `tests/unit/trigrep_cli_test.rs::rebuild_discards_the_old_cache`

### Requirement 5: A killed writer leaves a consistent cache [MUST]

A `trigrep index` process killed at any point, including inside its
commit, MUST leave a cache that reopens cleanly, passes `integrity_check`,
and holds either none or all of that run's files — never a partial set —
relying only on the pager's rollback journal. A following `index` MUST
complete on top of it.

**Implementation:** `src/bin/trigrep/index.rs::update`

#### Scenario: kill -9 across the commit windows

- GIVEN a tree indexed with `TRIGREP_CHUNK_FILES=100`
- WHEN the indexer is killed at random points
- THEN every committed prefix is a whole number of windows and a plain search on the half-built cache finishes the build before answering

#### Scenario: kill -9 across the commit window (single window)

- GIVEN a 400-file tree and the measured duration of one full index
- WHEN the indexer is `kill -9`ed at spread points in the second half of
  that duration, repeatedly
- THEN after every kill the cache opens, `integrity_check` is `ok`, the
  `files` count is 0 or 400, no non-empty hot journal remains, and a
  re-index succeeds

**Tests:** `tests/unit/trigrep_crash_test.rs::kill_9_mid_index_always_leaves_a_consistent_cache`

### Requirement 6: Ignore rules, binaries and symlinks [MUST]

Inside a git work tree the indexed set MUST be what git itself considers
not ignored (`.gitignore`, excludes, untracked-but-not-ignored included).
Files with a NUL in their first 8 KiB MUST be skipped, and symlinks MUST
be neither followed nor indexed.

**Implementation:** `src/bin/trigrep/walk.rs::list_files`

#### Scenario: .gitignore honored

- GIVEN a repo whose `.gitignore` lists a file and a directory, plus a
  tracked and an untracked file all containing the needle
- WHEN searching
- THEN only the tracked and untracked files are hits

**Tests:** `tests/unit/trigrep_cli_test.rs::gitignore_is_honored_inside_a_work_tree`

#### Scenario: Binary and symlink skipped

- GIVEN a text file, a binary file with a NUL byte, and a symlink to the
  text file, all containing the needle
- WHEN searching
- THEN exactly one hit, from the text file

**Tests:** `tests/unit/trigrep_cli_test.rs::binary_files_and_symlinks_are_skipped`

### Requirement 7: A plain search skips the freshness check once a cache exists [MUST]

Unless the cache for a root does not exist yet, `trigrep` MUST NOT walk
the filesystem, `stat`, hash, or write to the cache before a search — it
searches the cache exactly as it stands. The root's first-ever search
MUST still build the cache (there is nothing to search otherwise).
`-u`/`--update` MUST force the freshness check even when a cache already
exists, trading latency for currency; `trigrep index` MUST always refresh
regardless of this default, since bringing the cache up to date is its
entire purpose. A file present in the cache and unchanged on disk MUST
still be found normally, since the match itself always reads the real
file.

**Implementation:** `src/bin/trigrep/main.rs::open_and_update`

#### Scenario: A file added after the last build is invisible by default, unchanged files are not

- GIVEN a root indexed with one file, then a second file added afterward
  with the same searched content
- WHEN searching with no flags
- THEN only the originally indexed file is reported, the cache file's own
  mtime is unchanged by the search, and `-u`/`--update` catches up to
  report both

**Tests:** `tests/unit/trigrep_cli_test.rs::default_search_skips_the_freshness_check_once_a_cache_exists`

#### Scenario: The very first search still builds the cache

- GIVEN a fresh root with one file, never indexed
- WHEN a plain search runs with no cache yet present
- THEN it finds the match and the cache file exists afterwards

**Tests:** `tests/unit/trigrep_cli_test.rs::first_ever_search_still_builds_the_cache_even_by_default`

### Requirement 8: Terminal output is grouped and coloured; pipes stay flat and plain [MUST]

On a terminal, `trigrep` MUST group hits under one path heading per file
with hits, print `line:text` beneath, and separate files with a blank line;
files without hits MUST print nothing. `-f`/`--flatten` MUST force the
`path:line:text` form on a terminal. Colour (path magenta, line number
green, every matched span bold red, plain ANSI SGR) MUST be on when stdout
is a terminal and `NO_COLOR` is unset, off otherwise; `--color` MUST force
it on and `--no-color` off, both overriding `NO_COLOR` and the terminal
check. Stripping the escape sequences from coloured output MUST yield the
plain output byte for byte.

**Implementation:** `src/search.rs::Output`, `src/search.rs::run`

#### Scenario: Pipe, -f, --color, --no-color, NO_COLOR

- GIVEN two files with three matching lines, one line matching twice
- WHEN run with stdout piped
- THEN output is flat with no escape sequences, and `-f` produces the same bytes
- WHEN run with `--color`
- THEN the path, the line number and both spans on the double-match line carry SGR sequences, and stripping them yields the plain output
- WHEN run with `--no-color`, or with `NO_COLOR=1` and no flag
- THEN output equals the plain output; `--color` with `NO_COLOR=1` is still coloured

**Tests:** `tests/cli.rs::output_layout_and_color_flags`, `src/search.rs::tests::output_resolution_follows_the_pipe_convention`, `src/search.rs::tests::every_match_span_is_highlighted`
