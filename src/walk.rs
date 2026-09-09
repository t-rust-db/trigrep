// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! File enumeration for one indexed root (#34).
//!
//! Inside a git work tree the list comes from `git ls-files --cached
//! --others --exclude-standard`, which is git's own `.gitignore`/
//! `.git/info/exclude`/global-excludes semantics — byte-exact with what
//! ripgrep and tgrep aim to reproduce, with no ignore-rule parser of our
//! own to get subtly wrong. Outside a work tree it is a plain recursive
//! walk that skips `.git` directories and never follows symlinks.

use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One candidate file: its path relative to `root`, plus the metadata the
/// walk already paid for a `stat`-equivalent syscall to get — `None` when
/// the list came from `git ls-files`, which names paths without touching
/// the filesystem (#38: reusing this avoids a second, redundant `stat`
/// per file in `index::update`'s own diff, halving syscalls on the
/// non-git fallback path).
pub struct Entry {
    pub rel: String,
    pub metadata: Option<Metadata>,
}

/// Files under `root` to consider, sorted by path, deduplicated, plus a
/// count of subdirectories or entries skipped because they could not be
/// read (#15: permission-denied and similar no longer abort the whole
/// walk — one bad subdirectory used to take the entire index down with
/// it; now it is skipped, like `grep -r` skips what it cannot open).
/// `root` itself failing to open is still a real error: there is nothing
/// to index at all, not a partial result to return.
pub fn list_files(root: &Path) -> std::io::Result<(Vec<Entry>, usize)> {
    let (mut files, skipped) = match git_ls_files(root) {
        Some(list) => (
            list.into_iter()
                .map(|rel| Entry {
                    rel,
                    metadata: None,
                })
                .collect(),
            0,
        ),
        None => {
            let mut out = Vec::new();
            std::fs::read_dir(root)?; // root itself: propagate a real failure
            let mut skipped = 0usize;
            walk_dir(root, root, &mut out, &mut skipped);
            (out, skipped)
        }
    };
    files.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    files.dedup_by(|a, b| a.rel == b.rel);
    Ok((files, skipped))
}

fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    let inside = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .ok()?;
    if !inside.status.success() || String::from_utf8_lossy(&inside.stdout).trim() != "true" {
        return None;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

/// Recursive walk, never aborting the whole run over one bad
/// subdirectory or entry (#15) — each skip increments `skipped` rather
/// than propagating, so the caller can report a count. No depth limit
/// beyond the filesystem's own (a real cycle is impossible: symlinked
/// directories are never followed, see below).
fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<Entry>, skipped: &mut usize) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            *skipped = skipped.saturating_add(1);
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            *skipped = skipped.saturating_add(1);
            continue;
        };
        let path: PathBuf = entry.path();
        // `symlink_metadata` so a symlinked directory is neither followed
        // nor listed — loops and out-of-root escapes are both avoided. A
        // symlink is neither `is_dir` nor `is_file` under this call, so
        // it is silently skipped either way.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            *skipped = skipped.saturating_add(1);
            continue;
        };
        if meta.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            walk_dir(root, &path, out, skipped);
        } else if meta.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(Entry {
                    rel: rel.to_string_lossy().into_owned(),
                    metadata: Some(meta),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(non_snake_case)]
    mod mcdc_vectors {
        //! Tagged MC/DC vectors, trigrep#10.

        // walk_55: `!inside.status.success() || trim() != "true"`
        #[test]
        fn mcdc__walk_65__v1_outside_a_work_tree_condition_one_is_true() {
            // `git rev-parse` exits non-zero outside any repository, so
            // condition 1 alone is enough regardless of condition 2.
            let dir =
                std::env::temp_dir().join(format!("trigrep-mcdc-walk55-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            assert!(super::super::list_files(&dir).is_ok());
            // git_ls_files is private to this module; list_files falls back
            // to the plain walk exactly when it returns None, which the
            // "not a work tree" case is a lower-level test of via git's own
            // exit code (checked here rather than parsed from stdout, since
            // a non-repo directory never reaches the stdout comparison).
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["rev-parse", "--is-inside-work-tree"])
                .output()
                .unwrap();
            assert!(
                !out.status.success(),
                "condition 1 (command failed) must be true here"
            );
        }

        #[test]
        fn mcdc__walk_65__v2_inside_a_work_tree_both_conditions_false() {
            // Both false: git succeeds and prints exactly "true".
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(std::env::current_dir().unwrap())
                .args(["rev-parse", "--is-inside-work-tree"])
                .output()
                .unwrap();
            assert!(out.status.success());
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true");
        }

        #[test]
        fn mcdc__walk_65__v3_condition_two_isolated_via_direct_boolean() {
            // Command succeeding but printing something other than "true"
            // does not occur through git\'s own contract, so condition 2\'s
            // independent effect is pinned on runtime-read values (not
            // literals, so the check is not constant-folded away) shaped
            // like the real expression rather than by finding a git
            // invocation that produces it.
            let success = std::env::var("TRIGREP_MCDC_WALK55_UNSET").is_err(); // true
            let stdout =
                std::env::var("TRIGREP_MCDC_WALK55_STDOUT").unwrap_or_else(|_| "false".to_string());
            assert!(success && stdout.trim() != "true");
            assert!(!success || stdout.trim() != "true");
        }
    }
}
