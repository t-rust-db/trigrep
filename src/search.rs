// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The query path (#34): pattern → required trigrams → candidate file
//! ids → regex over just those files. No VM, no SQL.
//!
//! Required trigrams come from the literal runs a match *must* contain:
//! the regex is parsed to `regex-syntax`'s HIR and every run of adjacent
//! literals inside a concatenation (or a non-optional group) contributes
//! its trigrams. Alternations, classes and optional repetitions
//! contribute nothing — that is conservative, never wrong: fewer required
//! trigrams means more candidates, and the regex decides on real bytes.
//! A pattern with no 3-byte literal run (or `-i`, since the index is
//! case-sensitive) falls back to scanning every indexed file.

use std::io::Write;

use regex::bytes::Regex;
use regex_syntax::hir::{Hir, HirKind};

use crate::cache::{Cache, Result};
use crate::codec;
use crate::index::{is_binary, load_files, lookup_file, lookup_postings, FileMeta};

/// Literal byte runs every match must contain.
pub fn required_literals(hir: &Hir) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    collect(hir, &mut out);
    out
}

fn collect(hir: &Hir, out: &mut Vec<Vec<u8>>) {
    match hir.kind() {
        HirKind::Literal(lit) => out.push(lit.0.to_vec()),
        HirKind::Capture(c) => collect(&c.sub, out),
        HirKind::Repetition(r) if r.min >= 1 => collect(&r.sub, out),
        HirKind::Concat(items) => {
            let mut run: Vec<u8> = Vec::new();
            for item in items {
                match item.kind() {
                    HirKind::Literal(lit) => run.extend_from_slice(&lit.0),
                    _ => {
                        if !run.is_empty() {
                            out.push(std::mem::take(&mut run));
                        }
                        collect(item, out);
                    }
                }
            }
            if !run.is_empty() {
                out.push(run);
            }
        }
        _ => {}
    }
}

/// Packed trigrams a match must contain, or `None` when the pattern
/// gives no usable narrowing (scan everything).
pub fn required_trigrams(pattern: &str, case_insensitive: bool) -> Result<Option<Vec<i64>>> {
    if case_insensitive {
        return Ok(None);
    }
    let hir = regex_syntax::Parser::new().parse(pattern)?;
    let mut trigrams: Vec<i64> = required_literals(&hir)
        .iter()
        .flat_map(|run| codec::unique_trigrams(run))
        .collect();
    trigrams.sort_unstable();
    trigrams.dedup();
    Ok(if trigrams.is_empty() {
        None
    } else {
        Some(trigrams)
    })
}

/// Candidate file ids: the intersection of every required trigram's
/// posting list, smallest first so the working set only shrinks.
pub fn candidates(cache: &Cache, trigrams: &[i64]) -> Result<Vec<i64>> {
    let mut lists: Vec<Vec<i64>> = Vec::with_capacity(trigrams.len());
    for &t in trigrams {
        let list = lookup_postings(cache, t)?;
        if list.is_empty() {
            return Ok(Vec::new());
        }
        lists.push(list);
    }
    lists.sort_by_key(Vec::len);
    let mut iter = lists.into_iter();
    let Some(mut acc) = iter.next() else {
        return Ok(Vec::new());
    };
    for list in iter {
        acc = codec::intersect(&acc, &list);
        if acc.is_empty() {
            break;
        }
    }
    Ok(acc)
}

pub struct Query<'a> {
    pub regex: Regex,
    pub root: &'a std::path::Path,
    pub trigrams: Option<Vec<i64>>,
}

/// Runs the query, printing `path:line:text` per match; returns how many
/// lines matched.
pub fn run(cache: &Cache, q: &Query<'_>, out: &mut impl Write) -> Result<usize> {
    let files: Vec<FileMeta> = match &q.trigrams {
        Some(t) => {
            let mut v = Vec::new();
            for id in candidates(cache, t)? {
                if let Some(f) = lookup_file(cache, id)? {
                    v.push(f);
                }
            }
            v
        }
        None => load_files(cache)?,
    };
    let cwd = std::env::current_dir().ok();
    let mut matched = 0usize;
    for f in files {
        let full = q.root.join(&f.path);
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        if is_binary(&bytes) {
            continue;
        }
        let shown = cwd
            .as_deref()
            .and_then(|c| full.strip_prefix(c).ok())
            .unwrap_or(&full)
            .display()
            .to_string();
        for (n, line) in bytes.split(|&b| b == b'\n').enumerate() {
            if q.regex.is_match(line) {
                matched = matched.saturating_add(1);
                write!(out, "{shown}:{}:", n.saturating_add(1))?;
                out.write_all(line.strip_suffix(b"\r").unwrap_or(line))?;
                out.write_all(b"\n")?;
            }
        }
    }
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lits(p: &str) -> Vec<Vec<u8>> {
        required_literals(&regex_syntax::Parser::new().parse(p).unwrap_or_else(|e| {
            // Test-only: a bad pattern is a bug in the test itself.
            unreachable!("{e}")
        }))
    }

    #[test]
    fn plain_literal_is_one_run() {
        assert_eq!(lits("hello"), vec![b"hello".to_vec()]);
    }

    #[test]
    fn classes_and_alternation_split_runs() {
        assert_eq!(lits("foo.bar"), vec![b"foo".to_vec(), b"bar".to_vec()]);
        assert_eq!(lits("foo(bar|baz)"), vec![b"foo".to_vec()]);
        assert_eq!(lits("foo(bar)+"), vec![b"foo".to_vec(), b"bar".to_vec()]);
        assert_eq!(lits("foo(bar)?"), vec![b"foo".to_vec()]);
    }

    #[test]
    fn required_trigrams_fallbacks() {
        assert!(matches!(required_trigrams("ab", false), Ok(None)));
        assert!(matches!(required_trigrams("abc", true), Ok(None)));
        assert!(matches!(required_trigrams("[a-z]+", false), Ok(None)));
        let t = required_trigrams("abcd", false).ok().flatten();
        assert_eq!(t, Some(vec![codec::pack(*b"abc"), codec::pack(*b"bcd")]));
        assert!(required_trigrams("(", false).is_err());
    }
}
