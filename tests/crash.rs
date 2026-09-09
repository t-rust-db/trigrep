// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `trigrep` crash safety (#34): `kill -9` the indexer at many points,
//! including inside its commit, and check that the cache always reopens
//! as a consistent SQLite database — either still empty (the kill beat
//! the commit) or fully indexed (it didn't), never anything in between.
//! Recovery is `Pager::open`'s hot-journal rollback; trigrep adds no
//! bookkeeping of its own, which is exactly what this proves.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use db_core::storage::row::btree::{count_table_rows, TableCursor};
use db_core::storage::row::integrity::run_integrity_check;
use db_core::storage::row::schema::read_schema;
use db_core::storage::row::vfs::UnixVfs;
use trigrep::cache::open_db;

const TG: &str = env!("CARGO_BIN_EXE_tg");

fn iterations() -> u32 {
    std::env::var("TRIGREP_TORTURE_ITERATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12)
}

fn scratch() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("sqlite-rs-trigrep-crash-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let root = dir.join("tree");
    let cache = dir.join("cache");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    // Enough distinct trigrams that the commit writes thousands of pages,
    // so a kill has a real window to land inside `flush`. Every file has
    // its own token set (no trigram shared by all 400 files) so posting
    // lists stay similar in size — see `trigrep_cli_test.rs::
    // mixed_size_posting_lists_split_correctly` for the db-storage split
    // bug that a heavily shared vocabulary trips today.
    for i in 0..400u32 {
        let mut text = String::new();
        for j in 0..200u32 {
            let tok = u64::from(i)
                .wrapping_mul(1_000_003)
                .wrapping_add(u64::from(j).wrapping_mul(7919));
            text.push_str(&format!("{tok:x} "));
            if j % 8 == 7 {
                text.push('\n');
            }
        }
        std::fs::write(root.join(format!("f{i}.txt")), text).unwrap();
    }
    (root, cache)
}

/// The first token of f399.txt, as `scratch()` generates it.
fn needle_in_last_file() -> String {
    let tok = 399u64.wrapping_mul(1_000_003);
    format!("{tok:x} ")
}

fn indexer(root: &Path, cache: &Path) -> Command {
    let mut c = Command::new(TG);
    c.env(trigrep::index::CHUNK_FILES_ENV, "100")
        .env("TRIGREP_CACHE_DIR", cache)
        .arg("index")
        .arg(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

fn cache_file(cache: &Path) -> PathBuf {
    std::fs::read_dir(cache)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "db"))
        .expect("cache file exists")
}

/// Opens, integrity-checks, and returns the `files` row count.
fn recovered_file_count(path: &Path) -> i64 {
    let (header, pager) = open_db(&UnixVfs, path)
        .unwrap_or_else(|e| panic!("cache failed to reopen after kill: {e}"));
    let problems = run_integrity_check(&pager, &header, false);
    assert_eq!(problems, ["ok"], "integrity_check after kill: {problems:?}");
    let mut cursor = TableCursor::new(&pager, &header, 1);
    let schemas = read_schema(&mut cursor, header.text_encoding).unwrap();
    let files = schemas
        .iter()
        .find(|s| s.name == "files")
        .expect("files table");
    count_table_rows(&&pager, files.root_page).unwrap()
}

#[test]
fn kill_9_mid_index_always_leaves_a_consistent_cache() {
    let (root, cache) = scratch();

    // Calibrate: an uncontended run's wall time bounds where kills land.
    let start = Instant::now();
    let status = indexer(&root, &cache).status().unwrap();
    assert!(status.success());
    let full = start.elapsed();
    let db = cache_file(&cache);
    let total = recovered_file_count(&db);
    assert_eq!(total, 400);
    let journal = {
        let mut s = db.as_os_str().to_owned();
        s.push("-journal");
        PathBuf::from(s)
    };

    let (mut saw_empty, mut saw_full, mut saw_none) = (0u32, 0u32, 0u32);
    for i in 0..iterations() {
        std::fs::remove_file(&db).unwrap();
        std::fs::remove_file(&journal).ok();
        // Spread kills over the back half of the run, where the commit
        // (journal write, page writes, journal delete) happens.
        let frac = 0.5 + 0.5 * f64::from(i) / f64::from(iterations());
        let delay = full.mul_f64(frac);
        let mut child = indexer(&root, &cache).spawn().unwrap();
        std::thread::sleep(delay);
        child
            .kill()
            .unwrap_or_else(|e| panic!("iteration {i}: kill: {e}"));
        child.wait().ok();

        if !db.exists() {
            saw_none = saw_none.saturating_add(1);
            continue; // killed before the file was even created
        }
        let n = recovered_file_count(&db);
        if n == 0 {
            saw_empty = saw_empty.saturating_add(1);
        } else {
            saw_full = saw_full.saturating_add(1);
        }
        // Chunked commits (#3): every committed prefix is a whole number
        // of 100-file windows, never a torn one.
        assert!(
            n % 100 == 0 || n == total,
            "iteration {i} (killed at {delay:?}): {n} of {total} files committed — torn chunk"
        );
        if n > 0 && n < total {
            // A cache killed between chunks carries the in-progress marker,
            // so a plain search (no -u) must finish the update first rather
            // than answer from the partial index: f399 is always in the
            // last window.
            let out = Command::new(TG)
                .env(trigrep::cache::CACHE_DIR_ENV, &cache)
                .env(trigrep::index::CHUNK_FILES_ENV, "100")
                .arg(needle_in_last_file())
                .arg(&root)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .unwrap();
            assert_eq!(
                out.status.code(),
                Some(0),
                "iteration {i}: search on a half-built cache did not resume the build: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                recovered_file_count(&db),
                total,
                "iteration {i}: marker not honoured"
            );
        }
        if journal.exists() {
            let size = std::fs::metadata(&journal).unwrap().len();
            assert_eq!(
                size, 0,
                "iteration {i}: hot journal left behind after recovery"
            );
        }

        // And a survivor can finish the job on top of whatever was left.
        let status = indexer(&root, &cache).status().unwrap();
        assert!(
            status.success(),
            "iteration {i}: re-index after kill failed"
        );
        assert_eq!(recovered_file_count(&db), total, "iteration {i}");
    }
    eprintln!("kills landed: {saw_none} before create, {saw_empty} before commit, {saw_full} after commit");
    assert!(
        saw_empty.saturating_add(saw_full) > 0,
        "every kill beat file creation; calibration is off"
    );
    std::fs::remove_dir_all(root.parent().unwrap()).ok();
}
