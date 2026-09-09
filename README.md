# trigrep — `tg`

Serverless trigram-indexed grep. The problem [microsoft/tgrep](https://github.com/microsoft/tgrep)
solves — regex search over a large tree without re-reading every file — with
the shape `sqlite3` has and tgrep does not: **one cache file per indexed root,
opened per invocation, brought up to date, queried, closed.** No daemon to keep
warm, no JSON-RPC, no index format of its own.

The cache is a real SQLite-format database maintained through
[`db-storage`](https://github.com/t-rust-db/db-storage)'s b-tree and pager
(rollback journal, hot-journal recovery on open). Open it with `sqlite3` and
look at the `files`, `trigrams` and `meta` tables. No SQL runs: trigram lookup,
posting-list intersection and the regex are plain control flow over
`TableCursor::seek`, so `db-core`'s parser and VM are not involved.

Part of the [t-rust-db](https://github.com/t-rust-db) family. Extracted from
`sqlite-rs` (its former `sqlgrep` binary, ADR-0043 there) on 2026-09-09 so
that the library keeps zero third-party runtime dependencies and the tool has
a name that does not collide with [eirtools/sqlgrep](https://github.com/eirtools/sqlgrep),
which greps *inside* SQLite databases — a different job, now planned as
`sqlite-rs grep`.

## Usage

```bash
tg 'fn insert_row' .        # search (builds the cache on a root's first-ever search)
tg -i 'todo' src            # case-insensitive (scans every indexed file)
tg -u 'fn insert_row' .     # refresh the cache first — slower, catches recent edits
tg index .                  # bring the cache up to date without searching
tg index --rebuild .        # start over (also compacts stale postings)
tg cache-path .             # where this root's cache file lives
```

A plain search never re-scans the filesystem when a cache already exists:
on a large, mostly static tree the per-file `stat` used to dominate query
latency. `-u`/`--update` asks for the refresh; `tg index` does it explicitly.

Inside a git work tree `.gitignore` is honoured via `git ls-files
--exclude-standard` (git is a runtime dependency there); outside, a plain walk
that skips `.git` and never follows symlinks. Binaries (NUL in the first
8 KiB), symlinks and files over 64 MiB are skipped. Exit codes follow grep:
0 matched, 1 nothing, 2 error.

The cache lives at `$TRIGREP_CACHE_DIR/<key>.db`, or by default
`~/.cache/trigrep/` on Linux and `~/Library/Caches/trigrep/` on macOS, one
file per canonicalised root. A modified or deleted file leaves stale
posting-list entries that are filtered at query time (the regex always runs on
the real file); `--rebuild` reclaims them.

## How the index narrows a query

Required literals are extracted from the regex HIR: runs of adjacent literals
inside concatenations and non-optional groups. Classes, alternations and
optional repetitions contribute nothing (conservative: more candidates, never
fewer). A pattern with no 3-byte literal run, or a `-i` search (the index is
case-sensitive), scans every indexed file — the same limitation tgrep has.

## Building

```bash
make build        # or: cargo build
make test         # unit + CLI + crash-recovery tests
make lint         # clippy -D warnings, rustfmt --check
make install      # `tg` into ~/.cargo/bin
```

`.cargo/config.toml` patches `db-storage` to the sibling checkout for local
development; a clone without that sibling builds from the tagged git
dependency in `Cargo.toml`.

## Design

- [ADR 0001](.openspec/adr/0001-serverless-trigram-cache.md) — why a per-root
  cache file and not a daemon, and how the b-tree layer is used.
- [Spec 001](.openspec/specs/001-trigrep/spec.md) — requirements and scenarios.
