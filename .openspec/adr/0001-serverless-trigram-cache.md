# 0001 — `trigrep` (`tg`): a serverless trigram grep over a per-root db-storage cache file

> Carried over from sqlite-rs ADR-0043 (2026-09-08) when the binary moved to
> its own repository on 2026-09-09. Decision 6 below (amending sqlite-rs
> ADR-0040) no longer applies: this is a binary crate and declares its
> dependencies plainly. Names updated (`sqlgrep` → `trigrep`/`tg`).

**Status:** Accepted · **Date:** 2026-09-08 · Amends decision 2 of ADR-0040

## Context

#34 asks for a second binary applying this crate's own philosophy (embedded,
single file, no daemon) to microsoft/tgrep's problem: trigram-indexed grep
over a large tree. tgrep keeps a long-lived server holding a mmap'd index
plus an in-memory overlay, reached over JSON-RPC. The `sqlite3` shape is the
alternative: one cache file per indexed root, opened per invocation,
brought up to date, queried, closed.

The cache needs ordered key→blob storage with crash-safe batched updates.
That is exactly `db-storage`'s `row::btree` + `Pager` (rollback journal,
hot-journal recovery on open), and using it means the cache is a real,
`sqlite3`-inspectable database with no format of its own. No SQL is
involved: trigram lookup, posting-list intersection and the regex are plain
control flow, so `db-core`'s parser/VDBE is bypassed entirely.

Two things the issue text assumed do not hold in db-storage v0.6.2 and shaped
the design: the b-tree has no in-place update (only `delete_row` +
`insert_row`), and there is no explicit begin-transaction call (a
transaction is implicit from the first page mutation; `Pager::flush` is the
commit, `rollback` the abort).

ADR-0040 decision 2 says this crate declares no third-party runtime
dependencies of its own. A grep needs a regex engine, and a portable cache
location needs the XDG lookup.

## Decision

1. **`trigrep` is a `[[bin]]` in this crate**, `src/bin/trigrep/`, behind a
   `trigrep` cargo feature that is on by default (like `cli`). It depends on
   `sqlite_rs::{btree, pager, record, schema, vfs, dump}` only — the same
   re-exported db-storage layer the VDBE sits on.
2. **One SQLite-format cache file per canonicalized root**, at
   `$TRIGREP_CACHE_DIR` or `dirs::cache_dir()/trigrep/<fnv1a64(root)>.db`.
   Three rowid tables created through the b-tree layer and registered in
   `sqlite_master` with real DDL text: `meta(key, value)`, `files(path,
   mtime, size, hash)` (rowid = file id) and `trigrams(postings BLOB)`
   (rowid = the 3 bytes packed big-endian; payload a delta-varint posting
   list of file ids, tgrep's encoding).
3. **Incremental update is one `Pager` transaction per invocation.** Files
   whose mtime and size are unchanged are skipped; otherwise a 64-bit FNV-1a
   content hash decides. A file keeps its id for life. A modified file only
   *adds* the trigrams it did not have; lost trigrams and deleted files stay
   in posting lists as tombstones, filtered at query time (an id without a
   `files` row is skipped, and the regex always runs on the real file).
   Posting-list rewrites are batched per touched trigram, one
   `delete_row`+`insert_row` each. `--rebuild` compacts by starting over.
   Crash safety is `Pager::open`'s hot-journal recovery; trigrep adds no
   bookkeeping.
4. **`.gitignore` is honored by asking git.** Inside a work tree the file
   list is `git ls-files -z --cached --others --exclude-standard`; outside,
   a plain walk that skips `.git` and never follows symlinks. Files with a
   NUL in their first 8 KiB or larger than 64 MiB are skipped.
5. **Query narrowing uses required literals of the regex HIR**: runs of
   adjacent literals inside concatenations and non-optional groups. Classes,
   alternations and optional repetitions contribute nothing (conservative:
   more candidates, never fewer). Patterns without a 3-byte literal run, and
   `-i` searches (the index is case-sensitive), scan every indexed file.
6. **ADR-0040 decision 2 is amended:** a *binary-only, feature-gated*
   third-party dependency is allowed when the library would otherwise have
   to grow a general-purpose subsystem. `regex`, `regex-syntax` and `dirs`
   are declared optional under the `trigrep` feature; `default-features =
   false` consumers still see zero third-party crates. ADR-0030 holds: none
   of them are proc-macro crates. They go through `deny.toml`, `cargo vet`
   exemptions and both SBOMs like everything else.

## Alternatives rejected

- **A daemon holding the index (tgrep's design).** The problem the ticket
  exists to avoid; a process to keep warm is an ops concern SQLite has none
  of.
- **A bespoke index file format.** Would need its own crash-safety story;
  the pager's rollback journal already has one that real `sqlite3` also
  honours.
- **Going through SQL (`db-core`).** Nothing here needs a parser, planner
  or VM; a `TableCursor::seek` per trigram is the whole query plan.
- **Reassigning a modified file a new id and purging the old one.** Finding
  every posting list an id is in is a full scan of `trigrams` per changed
  file — quadratic in practice.
- **An in-crate `.gitignore` parser or the `ignore` crate.** The former is a
  large surface to get subtly wrong; the latter is a further closure
  (`globset`, `walkdir`, crossbeam) for something `git` already answers
  exactly.
- **Putting trigrep in its own repository.** It is an application over
  db-storage exactly as the `sqlite-rs` binary is over db-cli; a separate
  crate would re-pin the same storage tag and duplicate the supply-chain
  gates.

## Consequences

- `sqlite-rs.cdx.json` gains the `regex` closure (`regex`,
  `regex-automata`, `regex-syntax`, `aho-corasick`, `memchr`) and `dirs`
  (already present via db-cli) as production components under the default
  feature set; the README's zero-third-party claim now reads "for the
  library".
- Posting lists grow monotonically between `--rebuild`s; a heavily churned
  tree pays for tombstones at query time. A `stale` counter in `meta` plus
  automatic compaction is the natural follow-up.
- Indexing surfaced a db-storage v0.6.2 defect (t-rust-db/db-storage#31):
  `insert_into_leaf` split a full leaf by cell *count*, so mixed cell sizes
  handed the right half more bytes than a page holds and `write_leaf_page`
  wrapped silently. Fixed in v0.6.3 (byte-balanced split, up to three ways;
  overfull writes refused), which this crate pins from the start;
  `tests/unit/trigrep_cli_test.rs::mixed_size_posting_lists_split_correctly`
  stays as the regression pin.
- Slice 1 (#34) ships `index`, search, `cache-path`, `--rebuild`, `-i`.
  Not yet: case-insensitive narrowing, auto-compaction, chunked commits for
  very large first builds, a shared `trigrams()` helper in db-core.
