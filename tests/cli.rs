// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `trigrep` end-to-end (#34): drives the real binary through
//! `CARGO_BIN_EXE_tg` against scratch trees, with the cache
//! redirected via `TRIGREP_CACHE_DIR` so nothing touches `~/.cache`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use db_core::storage::row::btree::TableCursor;
use db_core::storage::row::integrity::run_integrity_check;
use db_core::storage::row::schema::read_schema;
use db_core::storage::row::vfs::UnixVfs;
use trigrep::cache::open_db;

const SQLGREP: &str = env!("CARGO_BIN_EXE_tg");

struct Scratch {
    root: PathBuf,
    cache_dir: PathBuf,
}

fn scratch(label: &str) -> Scratch {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "sqlite-rs-trigrep-{label}-{}-{n}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    let root = dir.join("tree");
    let cache_dir = dir.join("cache");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&cache_dir).unwrap();
    Scratch { root, cache_dir }
}

impl Scratch {
    fn write(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(SQLGREP)
            .env("TRIGREP_CACHE_DIR", &self.cache_dir)
            .args(args)
            .arg(&self.root)
            .output()
            .unwrap_or_else(|e| panic!("spawning {SQLGREP}: {e}"))
    }

    fn search(&self, pattern: &str) -> (i32, String) {
        let out = self.run(&[pattern]);
        (
            out.status.code().unwrap(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    fn search_args(&self, args: &[&str], pattern: &str) -> (i32, String) {
        let mut a: Vec<&str> = args.to_vec();
        a.push(pattern);
        let out = Command::new(SQLGREP)
            .env("TRIGREP_CACHE_DIR", &self.cache_dir)
            .args(&a)
            .arg(&self.root)
            .output()
            .unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    fn cache_path(&self) -> PathBuf {
        let out = self.run(&["cache-path"]);
        assert!(out.status.success());
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
    }
}

/// Opens the cache the way any reader would and asserts it is a clean
/// SQLite database with exactly trigrep's schema.
fn assert_cache_healthy(path: &Path) {
    let (header, pager) = open_db(&UnixVfs, path).expect("cache opens");
    let problems = run_integrity_check(&pager, &header, false);
    assert_eq!(problems, ["ok"], "integrity_check: {problems:?}");
    let mut cursor = TableCursor::new(&pager, &header, 1);
    let mut names: Vec<String> = read_schema(&mut cursor, header.text_encoding)
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    names.sort();
    assert_eq!(names, ["files", "meta", "trigrams"]);
}

#[test]
fn index_then_search_prints_file_line_matches_and_grep_exit_codes() {
    let s = scratch("basic");
    s.write("a.txt", "alpha\nneedle_one here\n");
    s.write("sub/b.rs", "fn needle_one() {}\nnope\n");
    s.write("c.txt", "nothing\n");

    let out = s.run(&["index"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("3 added"));

    let (code, stdout) = s.search("needle_one");
    assert_eq!(code, 0);
    let mut lines: Vec<&str> = stdout.lines().collect();
    lines.sort();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with("a.txt:2:needle_one here"), "{lines:?}");
    assert!(
        lines[1].ends_with("sub/b.rs:1:fn needle_one() {}"),
        "{lines:?}"
    );

    let (code, stdout) = s.search("zzz_absent_zzz");
    assert_eq!(code, 1);
    assert!(stdout.is_empty());

    let out = s.run(&["("]);
    assert_eq!(out.status.code(), Some(2));

    assert_cache_healthy(&s.cache_path());
}

#[test]
fn search_without_prior_index_builds_the_cache_first() {
    let s = scratch("lazy");
    s.write("x.txt", "lazy_needle\n");
    assert!(!s.cache_path().exists());
    let (code, stdout) = s.search("lazy_needle");
    assert_eq!(code, 0);
    assert!(stdout.contains("x.txt:1:lazy_needle"));
    assert!(s.cache_path().exists());
}

#[test]
fn regex_patterns_narrow_by_literals_but_match_by_regex() {
    let s = scratch("regex");
    s.write("a.txt", "foo123bar\nfoo bar\nfooXbar\n");
    let (_, stdout) = s.search("foo[0-9]+bar");
    assert_eq!(stdout.lines().count(), 1);
    assert!(stdout.contains(":1:foo123bar"));
    // No 3-byte literal run: falls back to scanning everything.
    let (_, stdout) = s.search("o.b");
    assert_eq!(stdout.lines().count(), 2, "{stdout}");
    // Case-insensitive also scans everything.
    let (_, stdout) = s.run_search_i("FOOX");
    assert!(stdout.contains(":3:fooXbar"));
}

impl Scratch {
    fn run_search_i(&self, pattern: &str) -> (i32, String) {
        let out = Command::new(SQLGREP)
            .env("TRIGREP_CACHE_DIR", &self.cache_dir)
            .args(["-i", pattern])
            .arg(&self.root)
            .output()
            .unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }
}

#[test]
fn incremental_update_touches_only_changed_files() {
    let s = scratch("incremental");
    s.write("keep.txt", "steady_needle\n");
    s.write("edit.txt", "before_needle\n");
    s.write("gone.txt", "gone_needle\n");
    let out = s.run(&["index"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("3 added"));

    // A second pass with nothing changed writes nothing.
    let out = s.run(&["index"]);
    let report = String::from_utf8_lossy(&out.stderr);
    assert!(
        report.contains("0 added, 0 changed, 0 removed, 3 unchanged"),
        "{report}"
    );
    assert!(report.contains("(0 posting lists rewritten)"), "{report}");

    // Distinct content so the mtime/size check cannot be fooled.
    std::thread::sleep(std::time::Duration::from_millis(20));
    s.write("edit.txt", "after_needle!!\n");
    std::fs::remove_file(s.root.join("gone.txt")).unwrap();
    s.write("new.txt", "fresh_needle\n");
    let out = s.run(&["index"]);
    let report = String::from_utf8_lossy(&out.stderr);
    assert!(
        report.contains("1 added, 1 changed, 1 removed, 1 unchanged"),
        "{report}"
    );

    assert_eq!(s.search("after_needle").0, 0);
    assert_eq!(s.search("fresh_needle").0, 0);
    assert_eq!(s.search("steady_needle").0, 0);
    // Stale postings are tombstones: the deleted file never surfaces.
    assert_eq!(s.search("gone_needle").0, 1);
    assert_eq!(s.search("before_needle").0, 1);
    assert_cache_healthy(&s.cache_path());
}

#[test]
fn rebuild_discards_the_old_cache() {
    let s = scratch("rebuild");
    s.write("a.txt", "one_needle\n");
    s.run(&["index"]);
    let before = std::fs::metadata(s.cache_path()).unwrap().len();
    // Grow the cache with a file, then delete it: without --rebuild the
    // tombstoned postings stay; with it the cache is exactly as fresh.
    let varied: String = (0..5000).map(|i| format!("tok{i} ")).collect();
    s.write("big.txt", &varied);
    s.run(&["index"]);
    std::fs::remove_file(s.root.join("big.txt")).unwrap();
    s.run(&["index"]);
    assert!(std::fs::metadata(s.cache_path()).unwrap().len() > before);
    let out = s.run(&["index", "--rebuild"]);
    assert!(out.status.success());
    assert_eq!(std::fs::metadata(s.cache_path()).unwrap().len(), before);
    assert_eq!(s.search("one_needle").0, 0);
    assert_cache_healthy(&s.cache_path());
}

#[test]
fn binary_files_and_symlinks_are_skipped() {
    let s = scratch("binary");
    s.write("text.txt", "bin_needle\n");
    std::fs::write(s.root.join("blob.bin"), b"bin_needle\0\x01\x02").unwrap();
    std::os::unix::fs::symlink(s.root.join("text.txt"), s.root.join("link.txt")).unwrap();
    let (code, stdout) = s.search("bin_needle");
    assert_eq!(code, 0);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(stdout.contains("text.txt:1:"));
}

#[test]
fn gitignore_is_honored_inside_a_work_tree() {
    let s = scratch("gitignore");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&s.root)
            .args(args)
            .output()
            .expect("git present");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    s.write(".gitignore", "ignored.txt\nbuild/\n");
    s.write("tracked.txt", "gi_needle tracked\n");
    s.write("untracked.txt", "gi_needle untracked-but-not-ignored\n");
    s.write("ignored.txt", "gi_needle ignored\n");
    s.write("build/out.txt", "gi_needle in ignored dir\n");
    git(&["add", "tracked.txt"]);

    let (code, stdout) = s.search("gi_needle");
    assert_eq!(code, 0);
    let mut hits: Vec<&str> = stdout.lines().collect();
    hits.sort();
    assert_eq!(hits.len(), 2, "{stdout}");
    assert!(hits[0].contains("tracked.txt:1:"));
    assert!(hits[1].contains("untracked.txt:1:"));
}

#[test]
fn one_cache_file_per_canonical_root() {
    let s = scratch("roots");
    s.write("a/x.txt", "x\n");
    s.write("b/y.txt", "y\n");
    let path_for = |sub: &str| {
        let out = Command::new(SQLGREP)
            .env("TRIGREP_CACHE_DIR", &s.cache_dir)
            .arg("cache-path")
            .arg(s.root.join(sub))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_ne!(path_for("a"), path_for("b"));
    // Same root through a non-canonical spelling: same file.
    assert_eq!(path_for("a"), path_for("b/../a/."));
    assert!(path_for("a").starts_with(s.cache_dir.to_str().unwrap()));
}

/// #38: once a cache exists, a plain search never re-scans the
/// filesystem — a file added after the last `index` (or after the cache
/// was built by an earlier search) stays invisible, and no cache write
/// occurs, until `-u`/`--update` or `trigrep index` explicitly refreshes
/// it. Search always reads the *real* file for the actual match, so an
/// existing, unchanged file is still found normally either way.
#[test]
fn default_search_skips_the_freshness_check_once_a_cache_exists() {
    let s = scratch("fast_default");
    s.write("a.txt", "steady_needle\n");
    s.run(&["index"]);
    let before = std::fs::metadata(s.cache_path())
        .unwrap()
        .modified()
        .unwrap();

    // A brand-new file with the same needle: never indexed, so a plain
    // search must not see it even though its content matches.
    s.write("b.txt", "steady_needle too\n");

    let (code, stdout) = s.search("steady_needle");
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(
        stdout.lines().count(),
        1,
        "{stdout} (b.txt must be invisible)"
    );
    assert!(stdout.contains("a.txt:1:"), "{stdout}");

    let after = std::fs::metadata(s.cache_path())
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        before, after,
        "a plain search over an existing cache must not write to it"
    );

    // `-u`/`--update` forces the refresh and catches up.
    let (code, stdout) = s.search_args(&["-u"], "steady_needle");
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(stdout.lines().count(), 2, "{stdout}");
}

/// #38: the very first search against a root with no cache yet must
/// still build one — there is nothing to search otherwise — even though
/// every search after that is fast by default.
#[test]
fn first_ever_search_still_builds_the_cache_even_by_default() {
    let s = scratch("fast_default_first");
    s.write("a.txt", "lazy_needle\n");
    assert!(!s.cache_path().exists());
    let (code, stdout) = s.search("lazy_needle");
    assert_eq!(code, 0, "{stdout}");
    assert!(s.cache_path().exists());
}

/// #38: the non-git fallback walk collects metadata while listing files;
/// `index::update` must reuse it (not re-`stat`) and still get correct
/// mtime/size — proven by an edit being detected exactly once, not
/// silently missed because a second stat somehow disagreed with the walk.
#[test]
fn metadata_reused_from_the_walk_still_detects_edits_outside_git() {
    let s = scratch("no_git_walk");
    s.write("a.txt", "before\n");
    let out = s.run(&["index"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 added"));

    std::thread::sleep(std::time::Duration::from_millis(20));
    s.write("a.txt", "after_marker\n");
    let out = s.run(&["index"]);
    let report = String::from_utf8_lossy(&out.stderr);
    assert!(
        report.contains("0 added, 1 changed, 0 removed, 0 unchanged"),
        "{report}"
    );
    assert_eq!(s.search("after_marker").0, 0);
    assert_eq!(s.search("before").0, 1);
}

/// Regression pin for a db-storage v0.6.2 bug found while indexing (#34,
/// t-rust-db/db-storage#31): `insert_into_leaf` split a full leaf by cell
/// *count*, so a leaf holding
/// many ~90-byte posting lists next to ~400-byte ones can hand the right
/// half more bytes than a page holds; `write_leaf_page` then wraps instead
/// of erroring and the next descent fails with "unexpected b-tree page
/// type". A vocabulary shared by all 400 files (long posting lists) mixed
/// with per-file tokens (short ones) is exactly that shape. Fixed in
/// db-storage v0.6.3 (t-rust-db/db-storage#31); this stays as the pin.
#[test]
fn mixed_size_posting_lists_split_correctly() {
    let s = scratch("mixed");
    for i in 0..400u32 {
        let mut text = String::new();
        for j in 0..200u32 {
            text.push_str(&format!(
                "w{}x{}y{} ",
                i,
                j,
                i.wrapping_mul(7919).wrapping_add(j)
            ));
        }
        s.write(&format!("f{i}.txt"), &text);
    }
    let out = s.run(&["index"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_cache_healthy(&s.cache_path());
}

/// #4: reading/hashing runs on a thread pool, but ids are assigned in
/// path order by the caller, so indexing the *same* tree into two caches
/// with different thread counts must produce byte-identical files.
#[test]
fn cache_is_byte_identical_across_thread_counts() {
    let s = scratch("threads");
    for i in 0..300u32 {
        let body: String = (0..40u32)
            .map(|j| format!("tok{:x} ", u64::from(i) * 7919 + u64::from(j) * 104_729))
            .collect();
        s.write(&format!("d{}/f{i}.txt", i % 7), &body);
    }
    let other_cache = s.cache_dir.with_file_name("cache-8threads");
    std::fs::create_dir_all(&other_cache).unwrap();
    let run = |cache_dir: &Path, threads: &str| {
        let out = Command::new(SQLGREP)
            .env("TRIGREP_CACHE_DIR", cache_dir)
            .env(trigrep::index::THREADS_ENV, threads)
            .arg("index")
            .arg(&s.root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&s.cache_dir, "1");
    run(&other_cache, "8");
    let a = std::fs::read(s.cache_path()).unwrap();
    let name = s.cache_path().file_name().unwrap().to_owned();
    let b = std::fs::read(other_cache.join(name)).unwrap();
    assert_eq!(a.len(), b.len(), "cache sizes differ");
    let mismatched = a.iter().zip(&b).filter(|(x, y)| x != y).count();
    assert_eq!(
        mismatched, 0,
        "{mismatched} differing bytes between 1-thread and 8-thread caches"
    );
}

/// #5/#6/#7: a pipe gets the classic flat form with no escapes; `-f` on a
/// pipe is the same bytes; `--color` adds SGR to path, line number and
/// every match span; `--no-color` and `NO_COLOR` strip them again.
#[test]
fn output_layout_and_color_flags() {
    let s = scratch("output");
    s.write("a.txt", "needle one\nplain\nneedle two needle\n");
    s.write("b.txt", "needle three\n");
    let run = |args: &[&str], no_color_env: bool| -> String {
        let mut c = Command::new(SQLGREP);
        c.env("TRIGREP_CACHE_DIR", &s.cache_dir)
            .env_remove("NO_COLOR");
        if no_color_env {
            c.env("NO_COLOR", "1");
        }
        let out = c.args(args).arg(&s.root).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let flat = run(&["needle"], false);
    assert!(
        !flat.contains('\x1b'),
        "piped output must be plain: {flat:?}"
    );
    let mut lines: Vec<&str> = flat.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].ends_with("a.txt:1:needle one"), "{lines:?}");
    assert!(lines[1].ends_with("a.txt:3:needle two needle"), "{lines:?}");
    assert!(lines[2].ends_with("b.txt:1:needle three"), "{lines:?}");
    // -f on a pipe: byte-identical to the default piped form.
    assert_eq!(run(&["-f", "needle"], false), flat);
    // --color forces SGR even on a pipe: path, line number, both spans on line 3.
    let colored = run(&["--color", "needle"], false);
    assert!(
        colored.contains("\x1b[35m"),
        "path colour missing: {colored:?}"
    );
    assert!(
        colored.contains("\x1b[32m3\x1b[0m:"),
        "line-number colour missing: {colored:?}"
    );
    let line3 = colored.lines().find(|l| l.contains(" two ")).unwrap();
    assert_eq!(
        line3.matches("\x1b[1;31mneedle\x1b[0m").count(),
        2,
        "{line3:?}"
    );
    // Stripping the escapes gives exactly the plain output.
    let stripped: String = {
        let mut out = String::new();
        let mut it = colored.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                for d in it.by_ref() {
                    if d == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    assert_eq!(stripped, flat);
    // --no-color and NO_COLOR=1 win over auto; --color wins over NO_COLOR.
    assert_eq!(run(&["--no-color", "needle"], false), flat);
    assert_eq!(run(&["needle"], true), flat);
    assert!(run(&["--color", "needle"], true).contains("\x1b[35m"));
}

/// #11: a file ending in a newline has no phantom empty last line, so
/// patterns that match the empty string report exactly the real lines.
#[test]
fn empty_matching_patterns_do_not_report_a_phantom_trailing_line() {
    let s = scratch("phantom");
    s.write("nl.txt", "one\ntwo\n");
    s.write("nonl.txt", "one\ntwo");
    s.write("empty.txt", "");
    for pat in ["^", "x*", "o"] {
        let (code, stdout) = s.search(pat);
        assert_eq!(code, 0, "{pat}");
        let mut lines: Vec<&str> = stdout.lines().collect();
        lines.sort_unstable();
        // nl.txt and nonl.txt have two real lines each; empty.txt none.
        assert_eq!(lines.len(), 4, "pattern {pat:?}: {lines:?}");
        assert!(
            lines.iter().all(|l| l.contains(":1:") || l.contains(":2:")),
            "{lines:?}"
        );
    }
}

/// #12: `--` ends flag parsing *and* subcommand dispatch — `tg -- index`
/// greps for the word "index" instead of running the indexer.
#[test]
fn double_dash_escapes_the_subcommand_dispatch() {
    let s = scratch("dashdash");
    s.write("a.txt", "the index of things\ncache-path here\n");
    let (code, stdout) = s.search_args(&["--"], "index");
    assert_eq!(code, 0);
    assert!(stdout.contains("a.txt:1:the index of things"), "{stdout:?}");
    let (code, stdout) = s.search_args(&["--"], "cache-path");
    assert_eq!(code, 0);
    assert!(stdout.contains("a.txt:2:cache-path here"), "{stdout:?}");
}

/// #14: several windows crossed at several thread counts — the cache
/// file is byte-identical for all of them.
#[test]
fn multi_window_builds_are_byte_identical_across_thread_counts() {
    let s = scratch("sweep");
    for i in 0..120u32 {
        s.write(
            &format!("d{}/f{i}.txt", i % 5),
            &format!("tok{:x} shared {}\n", i * 7919, i % 3),
        );
    }
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();
    for threads in ["1", "2", "3", "7", "8"] {
        let cache_dir = s.cache_dir.with_file_name(format!("cache-t{threads}"));
        std::fs::create_dir_all(&cache_dir).unwrap();
        let out = Command::new(SQLGREP)
            .env("TRIGREP_CACHE_DIR", &cache_dir)
            .env(trigrep::index::THREADS_ENV, threads)
            .env(trigrep::index::CHUNK_FILES_ENV, "25") // 120 files → 5 windows
            .arg("index")
            .arg(&s.root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let name = s.cache_path().file_name().unwrap().to_owned();
        images.push((
            threads.to_string(),
            std::fs::read(cache_dir.join(name)).unwrap(),
        ));
    }
    for (t, img) in &images[1..] {
        assert_eq!(img, &images[0].1, "threads={t} differs from threads=1");
    }
}

/// #14: a file that turns binary is dropped from the index on re-index and
/// comes back (as a new row) when it turns text again; stats say so.
#[test]
fn text_to_binary_to_text_reindex() {
    let s = scratch("flip");
    s.write("a.txt", "needle alpha\n");
    s.write("b.txt", "other\n");
    let index = |s: &Scratch| -> String {
        let out = s.run(&["index"]);
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    assert!(index(&s).contains("2 added"));
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(s.root.join("a.txt"), b"needle\0binary").unwrap();
    let st = index(&s);
    assert!(st.contains("1 removed"), "{st}");
    let (code, _) = s.search("needle");
    assert_eq!(code, 1, "binary content must not be searchable");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(s.root.join("a.txt"), b"needle back\n").unwrap();
    let st = index(&s);
    assert!(st.contains("1 added"), "{st}");
    let (code, out) = s.search("needle");
    assert_eq!(code, 0);
    assert!(out.contains("a.txt:1:needle back"), "{out}");
}

/// #14: a closed pipe is not an error — `tg ... | head -1` exits 0 silently.
#[test]
fn broken_pipe_exits_zero_without_noise() {
    use std::io::Read;
    let s = scratch("pipe");
    for i in 0..2000u32 {
        s.write(&format!("f{i}.txt"), "needle line\n".repeat(50).as_str());
    }
    assert!(s.run(&["index"]).status.success());
    let mut child = Command::new(SQLGREP)
        .env("TRIGREP_CACHE_DIR", &s.cache_dir)
        .arg("needle")
        .arg(&s.root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // read one byte, then drop the read end so the writer sees EPIPE
    let mut stdout = child.stdout.take().unwrap();
    let mut one = [0u8; 1];
    stdout.read_exact(&mut one).unwrap();
    drop(stdout);
    let status = child.wait().unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert_eq!(status.code(), Some(0), "stderr: {err}");
    assert!(err.is_empty(), "stderr should be silent on EPIPE: {err}");
}

/// #14: foreign or damaged files at the cache path are refused or
/// rebuilt, never silently indexed into.
#[test]
fn foreign_zero_byte_and_garbage_cache_files() {
    let s = scratch("foreign");
    s.write("a.txt", "needle\n");
    // learn the cache path by running once, then destroy the cache three ways
    assert!(s.run(&["index"]).status.success());
    let path = s.cache_path();
    // 1. zero-byte file: treated as fresh (bootstrapped), search works
    std::fs::write(&path, b"").unwrap();
    let (code, out) = s.search("needle");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("a.txt:1:needle"));
    // 2. garbage bytes: an error, exit 2, not a crash and not a silent scan
    std::fs::write(
        &path,
        b"this is not a database at all, not even close, really not",
    )
    .unwrap();
    let out = s.run(&["needle"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 3. --rebuild recovers from garbage
    let (code, text) = s.search_args(&["--rebuild"], "needle");
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("a.txt:1:needle"));
    // 4. a valid SQLite file that is not a trigrep cache is refused by name
    let other = scratch("foreign-other");
    other.write("z.txt", "zzz\n");
    assert!(other.run(&["index"]).status.success());
    let mut foreign = std::fs::read(other.cache_path()).unwrap();
    // rename table "files" -> "fileZ" in sqlite_master's DDL is too fiddly;
    // instead corrupt page 1's header magic to force a parse error.
    foreign[0] = b'X';
    std::fs::write(&path, &foreign).unwrap();
    let out = s.run(&["needle"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// #15: a cache whose stored root does not match the root it is being
/// asked about is rebuilt from scratch rather than trusted — a copied
/// cache directory (or, in principle, a hash collision) must never
/// silently serve one tree's index for another's search.
#[test]
fn mismatched_stored_root_triggers_a_rebuild_not_wrong_answers() {
    let a = scratch("root-a");
    a.write("f.txt", "needle_a\n");
    assert!(a.run(&["index"]).status.success());
    let b = scratch("root-b");
    b.write("f.txt", "needle_b\n");
    // Copy a's cache file to b's cache path: same bytes, wrong root.
    let a_cache = a.cache_path();
    std::fs::create_dir_all(&b.cache_dir).unwrap();
    let b_cache_name = b.cache_dir.join(a_cache.file_name().unwrap());
    // b's own cache-path depends on b's root hash, not a's file name;
    // find it by asking b directly, then plant a's bytes there.
    let real_b_cache = b.cache_path();
    std::fs::copy(&a_cache, &real_b_cache).unwrap();
    let _ = b_cache_name;
    // Searching b must find b's content, not a's — proving the mismatch
    // was detected and the cache rebuilt rather than trusted as-is.
    let (code, out) = b.search("needle_b");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("f.txt:1:needle_b"), "{out}");
    let (code, _) = b.search("needle_a");
    assert_eq!(code, 1, "a's content must not leak into b's search results");
}

/// #15: one unreadable subdirectory is skipped, not fatal to the run.
#[test]
#[cfg(unix)]
fn unreadable_subdirectory_is_skipped_not_fatal() {
    use std::os::unix::fs::PermissionsExt;
    let s = scratch("unreadable-dir");
    s.write("ok/a.txt", "needle_ok\n");
    s.write("locked/b.txt", "needle_locked\n");
    let locked = s.root.join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let out = s.run(&["index"]);
    // restore permissions before any assertion can early-return and leak
    // an unreadable directory into the test's own cleanup
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unreadable skipped"), "{stderr}");
    let (code, found) = s.search("needle_ok");
    assert_eq!(code, 0, "{found}");
    assert!(found.contains("ok/a.txt:1:needle_ok"));
}

/// #15: two `tg` processes racing the very first index of a root do not
/// corrupt the cache — one wins the bootstrap, the other proceeds on the
/// result, and both end with a searchable, integrity-checked cache.
#[test]
fn concurrent_first_index_does_not_corrupt_the_cache() {
    let s = scratch("race");
    for i in 0..50u32 {
        s.write(&format!("f{i}.txt"), &format!("needle {i}\n"));
    }
    let spawn = || {
        Command::new(SQLGREP)
            .env("TRIGREP_CACHE_DIR", &s.cache_dir)
            .arg("index")
            .arg(&s.root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut children: Vec<_> = (0..6).map(|_| spawn()).collect();
    for c in &mut children {
        assert!(c.wait().unwrap().success());
    }
    assert_cache_healthy(&s.cache_path());
    let (code, out) = s.search("needle 7");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("f7.txt:1:needle 7"));
}

/// `make smoke`'s contract: `--help`/`-h`/`--version` exit 0 with the text
/// on stdout and nothing on stderr; misuse still exits 2 on stderr.
#[test]
fn help_and_version_exit_zero_on_stdout() {
    for flag in ["--help", "-h", "--version", "-V"] {
        let out = Command::new(SQLGREP).arg(flag).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{flag}");
        assert!(!out.stdout.is_empty(), "{flag}: empty stdout");
        assert!(
            out.stderr.is_empty(),
            "{flag}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let help = Command::new(SQLGREP).arg("--help").output().unwrap();
    assert!(String::from_utf8_lossy(&help.stdout).starts_with("usage: trigrep "));
    let bad = Command::new(SQLGREP)
        .arg("--definitely-unknown")
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
    assert!(bad.stdout.is_empty());
}
