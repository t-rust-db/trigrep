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

struct Args {
    rebuild: bool,
    case_insensitive: bool,
    update: bool,
    /// `-f`/`--flatten`: force `path:line:text` even on a terminal (#6).
    flatten: bool,
    /// `--color` / `--no-color`; `None` = auto (#7).
    color: Option<bool>,
    positional: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        rebuild: false,
        case_insensitive: false,
        update: false,
        flatten: false,
        color: None,
        positional: Vec::new(),
    };
    let mut literal_rest = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            _ if literal_rest => args.positional.push(a),
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
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("trigrep: {msg}");
            }
            return usage();
        }
    };
    match args.positional.first().map(String::as_str) {
        Some("index") => run_index(&args),
        Some("cache-path") => run_cache_path(&args),
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
    let (mut cache, _) = match cache::open(&root, args.rebuild) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    match index::update(&mut cache, &root) {
        Ok(stats) => {
            eprintln!(
                "{}: {} added, {} changed, {} removed, {} unchanged ({} posting lists rewritten)",
                cache.path.display(),
                stats.added,
                stats.changed,
                stats.removed,
                stats.unchanged,
                stats.trigrams_rewritten
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
fn open_and_update(root: &Path, rebuild: bool, force_update: bool) -> cache::Result<cache::Cache> {
    let (mut cache, fresh) = cache::open(root, rebuild)?;
    if fresh || force_update {
        index::update(&mut cache, root)?;
    }
    Ok(cache)
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
