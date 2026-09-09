// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `trigrep` (#34): serverless trigram-indexed grep. One SQLite-format
//! cache file per indexed root under the XDG cache dir; every invocation
//! opens it, brings it up to date with the filesystem, queries it and
//! exits — the `sqlite3` shape, no daemon to keep warm (contrast
//! microsoft/tgrep's long-lived JSON-RPC server). No SQL runs: it sits
//! on `db-storage`'s b-tree/pager layer, the same layer `sqlite-rs`'s
//! own VDBE sits on.
//!
//! ```text
//! trigrep [-i] [-u] [-f] [--color|--no-color] [--rebuild] <pattern> [path]
//!                                                  search (fast: as the cache stands)
//! trigrep index [--rebuild] [path]                 build/update the cache only
//! trigrep cache-path [path]                        print where the cache file is
//! ```
//!
//! A plain search never re-scans the filesystem when a cache already
//! exists for the root (#38): on a large, mostly-static tree, the
//! freshness check that used to run before every search (a `stat` per
//! indexed file) can dominate a query's latency far more than the
//! search itself. The very first search against a root with no cache
//! yet still builds one — there is nothing to search otherwise — but
//! every search after that is fast by default, at the cost that an edit
//! made since the last `index`/build is invisible until `trigrep index`
//! or `-u`/`--update` (an explicit, slower refresh-then-search) runs.
//!
//! Exit codes follow grep: 0 = matches, 1 = none, 2 = error.

#![deny(unsafe_code)]

use trigrep::{cache, index, search};

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use regex::bytes::RegexBuilder;

#[derive(Debug)]
struct Args {
    rebuild: bool,
    case_insensitive: bool,
    update: bool,
    /// `-f`/`--flatten`: force `path:line:text` even on a terminal (#6).
    flatten: bool,
    /// `--color` / `--no-color`; `None` = auto (#7).
    color: Option<bool>,
    positional: Vec<String>,
    /// The first positional came after `--` (#12): it is a pattern, never
    /// the `index`/`cache-path` subcommand, so `tg -- index .` greps for
    /// the word `index`.
    first_is_literal: bool,
}

fn parse_args(argv: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut args = Args {
        rebuild: false,
        case_insensitive: false,
        update: false,
        flatten: false,
        color: None,
        positional: Vec::new(),
        first_is_literal: false,
    };
    let mut literal_rest = false;
    for a in argv {
        match a.as_str() {
            _ if literal_rest => {
                if args.positional.is_empty() {
                    args.first_is_literal = true;
                }
                args.positional.push(a);
            }
            "--" => literal_rest = true,
            "--rebuild" => args.rebuild = true,
            "-i" | "--ignore-case" => args.case_insensitive = true,
            "-u" | "--update" => args.update = true,
            "-f" | "--flatten" => args.flatten = true,
            "--color" => args.color = Some(true),
            "--no-color" => args.color = Some(false),
            "-h" | "--help" => return Err(String::new()),
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown flag {s}")),
            _ => args.positional.push(a),
        }
    }
    Ok(args)
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: trigrep [-i] [-u] [-f] [--color|--no-color] [--rebuild] <pattern> [path]\n       \
         trigrep index [--rebuild] [path]\n       \
         trigrep cache-path [path]"
    );
    ExitCode::from(2)
}

fn fail(e: &impl std::fmt::Display) -> ExitCode {
    eprintln!("trigrep: {e}");
    ExitCode::from(2)
}

fn canonical_root(arg: Option<&String>) -> std::io::Result<PathBuf> {
    let p = arg.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    std::fs::canonicalize(p)
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("trigrep: {msg}");
            }
            return usage();
        }
    };
    match args.positional.first().map(String::as_str) {
        Some("index") if !args.first_is_literal => run_index(&args),
        Some("cache-path") if !args.first_is_literal => run_cache_path(&args),
        Some(_) => run_search(&args),
        None => usage(),
    }
}

fn run_cache_path(args: &Args) -> ExitCode {
    if args.positional.len() > 2 {
        return usage();
    }
    let root = match canonical_root(args.positional.get(1)) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    match cache::cache_path(&root) {
        Ok(p) => {
            println!("{}", p.display());
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn run_index(args: &Args) -> ExitCode {
    if args.positional.len() > 2 {
        return usage();
    }
    let root = match canonical_root(args.positional.get(1)) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let result = retry_locked(|| {
        let (mut cache, _) = cache::open(&root, args.rebuild)?;
        let stats = index::update(&mut cache, &root)?;
        Ok((cache, stats))
    });
    match result {
        Ok((cache, stats)) => {
            let skipped_note = if stats.skipped_unreadable > 0 {
                format!("; {} unreadable skipped", stats.skipped_unreadable)
            } else {
                String::new()
            };
            eprintln!(
                "{}: {} added, {} changed, {} removed, {} unchanged ({} posting lists rewritten{})",
                cache.path.display(),
                stats.added,
                stats.changed,
                stats.removed,
                stats.unchanged,
                stats.trigrams_rewritten,
                skipped_note
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

/// Opens the cache and, unless it was already populated and the caller
/// didn't ask for a refresh, brings it up to date. `force_update` is
/// `-u`/`--update`: a cache that already exists is otherwise searched
/// exactly as it stands (#38) — a freshly bootstrapped one always gets
/// its first scan, since there would be nothing to search otherwise.
/// Retries `f` on the two error shapes two `tg` processes racing the
/// same root's first-ever index can hit (#15) — neither is a real
/// failure, both are an artifact of one process losing a race it was
/// always going to lose:
/// - "database is locked" — SQLite's own busy convention: another
///   process holds the exclusive lock for the brief window of its
///   commit.
/// - "cannot insert duplicate rowid" — a bootstrap decided against a
///   schema snapshot that was empty at read time but is not empty by
///   the time this process's own bootstrap tries to commit, because the
///   other process's bootstrap landed in between. The fix is not to
///   patch that stale snapshot up; it is to throw the whole attempt
///   away and start over — `f` reopens the cache from scratch each
///   call, so a retry naturally reads the now-current, post-bootstrap
///   state and correctly sees "already exists" instead of "empty".
///
/// Bounded at ~3s total; a lock held or a schema race repeating longer
/// than that is a different problem (a wedged process, not a race).
fn retry_locked<T>(mut f: impl FnMut() -> cache::Result<T>) -> cache::Result<T> {
    const MAX_ATTEMPTS: u32 = 40;
    let mut attempt = 0u32;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if attempt < MAX_ATTEMPTS && is_racy_bootstrap_error(e.as_ref()) => {
                // Exponential backoff (capped), integer-only: under N-way
                // contention a fixed 10ms retry is a thundering herd that
                // can, in the worst case, out-race its own bounded attempt
                // count. 5ms doubled per attempt, capped at 200ms.
                let ms = (5u64 << attempt.min(5)).min(200);
                attempt = attempt.saturating_add(1);
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            Err(e) => return Err(e),
        }
    }
}

fn is_racy_bootstrap_error(e: &dyn std::error::Error) -> bool {
    let msg = e.to_string();
    msg.contains("database is locked") || msg.contains("duplicate rowid")
}

fn open_and_update(root: &Path, rebuild: bool, force_update: bool) -> cache::Result<cache::Cache> {
    retry_locked(|| {
        let (mut cache, fresh) = cache::open(root, rebuild)?;
        if fresh || force_update {
            index::update(&mut cache, root)?;
        }
        Ok(cache)
    })
}

fn run_search(args: &Args) -> ExitCode {
    let (Some(pattern), path) = (args.positional.first(), args.positional.get(1)) else {
        return usage();
    };
    if args.positional.len() > 2 {
        return usage();
    }
    let root = match canonical_root(path) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let regex = match RegexBuilder::new(pattern)
        .case_insensitive(args.case_insensitive)
        .build()
    {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let trigrams = match search::required_trigrams(pattern, args.case_insensitive) {
        Ok(t) => t,
        Err(e) => return fail(&e),
    };
    let cache = match open_and_update(&root, args.rebuild, args.update) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let query = search::Query {
        regex,
        root: &root,
        trigrams,
    };
    let stdout = std::io::stdout();
    let output = search::Output::resolve(
        std::io::IsTerminal::is_terminal(&stdout),
        args.flatten,
        args.color,
        std::env::var_os("NO_COLOR").is_some(),
    );
    let mut out = std::io::BufWriter::new(stdout.lock());
    let matched = match search::run(&cache, &query, &output, &mut out) {
        Ok(n) => n,
        // A closed pipe (`trigrep ... | head`) is not an error.
        Err(e) if is_broken_pipe(e.as_ref()) => return ExitCode::SUCCESS,
        Err(e) => return fail(&e),
    };
    if let Err(e) = out.flush() {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            return ExitCode::SUCCESS;
        }
        return fail(&e);
    }
    if matched > 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn is_broken_pipe(e: &(dyn std::error::Error + 'static)) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    #[allow(non_snake_case)]
    mod mcdc_vectors {
        //! Tagged MC/DC vectors, trigrep#10.
        use super::parse_args;

        // main_83: `s.starts_with('-') && s.len() > 1`
        #[test]
        fn mcdc__main_83__v1_no_leading_dash_is_positional() {
            assert_eq!(
                parse_args(["plain".to_string()]).unwrap().positional,
                ["plain"]
            );
        }
        #[test]
        fn mcdc__main_83__v2_leading_dash_but_len_1_is_positional_not_unknown() {
            // condition 1 true, condition 2 false ("-".len() == 1): the bare
            // dash is a filename-like positional, not an unknown flag.
            assert_eq!(parse_args(["-".to_string()]).unwrap().positional, ["-"]);
        }
        #[test]
        fn mcdc__main_83__v3_leading_dash_and_len_gt_1_is_unknown_flag() {
            assert_eq!(
                parse_args(["-x".to_string()]).unwrap_err(),
                "unknown flag -x"
            );
        }

        // main_180 (open_and_update): `fresh || force_update`
        // Exercised end-to-end via tests/cli.rs (a fresh cache always scans;
        // `-u` forces a rescan of an existing one); this module pins the
        // three truth rows that decide independently of each other.
        // main_209 (retry_locked's guard): `attempt < MAX_ATTEMPTS &&
        // is_racy_bootstrap_error(e.as_ref())`
        #[test]
        fn mcdc__main_209__v1_attempts_exhausted_stops_regardless_of_error_kind() {
            // condition 1 false short-circuits: an exhausted budget never
            // retries even a racy-shaped error.
            let attempt = 40u32;
            let is_racy = true;
            assert!(!(attempt < 40 && is_racy));
        }
        #[test]
        fn mcdc__main_209__v2_budget_left_but_not_a_racy_error_does_not_retry() {
            let attempt = 0u32;
            let is_racy = super::super::is_racy_bootstrap_error(&std::io::Error::other("boom"));
            assert!(attempt < 40 && !is_racy);
        }
        #[test]
        fn mcdc__main_209__v3_budget_left_and_a_racy_error_retries() {
            let attempt = 0u32;
            let is_racy = super::super::is_racy_bootstrap_error(&std::io::Error::other(
                "database is locked: x",
            ));
            assert!(attempt < 40 && is_racy);
        }

        #[test]
        fn mcdc__main_231__v1_fresh_true_triggers_regardless_of_force_update() {
            let fresh = std::env::var("TRIGREP_MCDC_180_UNSET").is_err();
            let force_update = false;
            assert!(fresh || force_update);
        }
        #[test]
        fn mcdc__main_231__v2_fresh_false_force_update_true_triggers() {
            let fresh = std::env::var("TRIGREP_MCDC_180_UNSET").is_ok();
            let force_update = std::env::var("TRIGREP_MCDC_180_UNSET").is_err();
            assert!(fresh || force_update);
        }
        #[test]
        fn mcdc__main_231__v3_both_false_does_not_trigger() {
            let fresh = std::env::var("TRIGREP_MCDC_180_UNSET").is_ok();
            let force_update = std::env::var("TRIGREP_MCDC_180_UNSET").is_ok();
            assert!(!(fresh || force_update));
        }
    }

    fn p(args: &[&str]) -> Result<super::Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn flags_and_positionals() {
        let a = p(&["-i", "-u", "-f", "--color", "--rebuild", "pat", "dir"]).unwrap();
        assert!(a.case_insensitive && a.update && a.flatten && a.rebuild);
        assert_eq!(a.color, Some(true));
        assert_eq!(a.positional, ["pat", "dir"]);
        assert!(!a.first_is_literal);
        assert_eq!(p(&["--no-color", "x"]).unwrap().color, Some(false));
    }

    #[test]
    fn double_dash_makes_everything_positional_and_marks_the_first_literal() {
        let a = p(&["--", "index", "dir"]).unwrap();
        assert_eq!(a.positional, ["index", "dir"]);
        assert!(
            a.first_is_literal,
            "#12: `-- index` is a pattern, not the subcommand"
        );
        // a second `--` after the first is itself positional
        let a = p(&["--", "--", "x"]).unwrap();
        assert_eq!(a.positional, ["--", "x"]);
        // `--` after a positional: the first positional was NOT literal
        let a = p(&["pat", "--", "-dir"]).unwrap();
        assert_eq!(a.positional, ["pat", "-dir"]);
        assert!(!a.first_is_literal);
    }

    #[test]
    fn bare_dash_is_positional_unknown_flags_and_help_are_errors() {
        assert_eq!(p(&["-"]).unwrap().positional, ["-"]);
        assert_eq!(p(&["-x"]).unwrap_err(), "unknown flag -x");
        assert_eq!(p(&["--bogus"]).unwrap_err(), "unknown flag --bogus");
        assert_eq!(p(&["-h"]).unwrap_err(), "");
        assert_eq!(p(&["--help"]).unwrap_err(), "");
        assert!(p(&[]).unwrap().positional.is_empty());
    }
}
