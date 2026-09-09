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

/// How hits are laid out (#5, #6). `Grouped` is ripgrep's terminal
/// form: one path heading per file with hits, `line:text` beneath, a
/// blank line between files. `Flat` is the classic `path:line:text`,
/// what a pipe gets and what `-f`/`--flatten` forces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Grouped,
    Flat,
}

/// Whether to emit ANSI SGR colour (#7). Resolved once in `main` from
/// `--color`/`--no-color`, `NO_COLOR` and whether stdout is a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    On,
    Off,
}

/// Plain SGR sequences, ripgrep's default palette: magenta paths, green
/// line numbers, bold red matches. Hand-written so this stays a crate
/// with no colour dependency.
const SGR_PATH: &[u8] = b"\x1b[35m";
const SGR_LINE: &[u8] = b"\x1b[32m";
const SGR_MATCH: &[u8] = b"\x1b[1;31m";
const SGR_RESET: &[u8] = b"\x1b[0m";

pub struct Output {
    pub layout: Layout,
    pub color: Color,
}

impl Output {
    /// Applies the pipe convention: grouped only on a terminal, colour
    /// only on a terminal unless forced. `flatten` and `color` are the
    /// explicit flags; `no_color_env` is `NO_COLOR` being set at all.
    pub fn resolve(is_tty: bool, flatten: bool, color: Option<bool>, no_color_env: bool) -> Output {
        let layout = if flatten || !is_tty {
            Layout::Flat
        } else {
            Layout::Grouped
        };
        let color = match color {
            Some(true) => Color::On,
            Some(false) => Color::Off,
            None if is_tty && !no_color_env => Color::On,
            None => Color::Off,
        };
        Output { layout, color }
    }

    fn paint(&self, out: &mut impl Write, sgr: &[u8], bytes: &[u8]) -> std::io::Result<()> {
        if self.color == Color::On {
            out.write_all(sgr)?;
            out.write_all(bytes)?;
            out.write_all(SGR_RESET)
        } else {
            out.write_all(bytes)
        }
    }

    /// Writes one matched line, highlighting every match span (#7).
    fn line(&self, out: &mut impl Write, regex: &Regex, line: &[u8]) -> std::io::Result<()> {
        if self.color == Color::Off {
            return out.write_all(line);
        }
        let mut at = 0usize;
        for m in regex.find_iter(line) {
            if m.start() < at {
                continue; // find_iter yields non-overlapping matches; guard anyway
            }
            out.write_all(&line[at..m.start()])?;
            if m.end() > m.start() {
                self.paint(out, SGR_MATCH, &line[m.start()..m.end()])?;
            }
            at = m.end();
        }
        out.write_all(&line[at..])
    }
}

/// Runs the query, printing hits in `o`'s layout; returns how many lines
/// matched.
pub fn run(cache: &Cache, q: &Query<'_>, o: &Output, out: &mut impl Write) -> Result<usize> {
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
    let mut files_with_hits = 0usize;
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
        let mut heading_written = false;
        // A file ending in '\n' splits into a trailing empty segment that
        // is not a line; without this, `^` or `x*` reported a phantom
        // last line (#11). A file without a trailing newline keeps its
        // real last line.
        let body = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        let lines = if body.is_empty() && bytes.is_empty() {
            &bytes[..0]
        } else {
            body
        };
        for (n, line) in lines.split(|&b| b == b'\n').enumerate() {
            if lines.is_empty() {
                break;
            }
            if !q.regex.is_match(line) {
                continue;
            }
            matched = matched.saturating_add(1);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let lineno = n.saturating_add(1).to_string();
            match o.layout {
                Layout::Flat => {
                    o.paint(out, SGR_PATH, shown.as_bytes())?;
                    out.write_all(b":")?;
                    o.paint(out, SGR_LINE, lineno.as_bytes())?;
                    out.write_all(b":")?;
                }
                Layout::Grouped => {
                    if !heading_written {
                        if files_with_hits > 0 {
                            out.write_all(b"\n")?;
                        }
                        o.paint(out, SGR_PATH, shown.as_bytes())?;
                        out.write_all(b"\n")?;
                        heading_written = true;
                        files_with_hits = files_with_hits.saturating_add(1);
                    }
                    o.paint(out, SGR_LINE, lineno.as_bytes())?;
                    out.write_all(b":")?;
                }
            }
            o.line(out, &q.regex, line)?;
            out.write_all(b"\n")?;
        }
    }
    Ok(matched)
}

#[cfg(test)]
mod tests {

    #[test]
    fn output_resolution_follows_the_pipe_convention() {
        // tty, nothing forced: grouped + colour
        let o = Output::resolve(true, false, None, false);
        assert_eq!((o.layout, o.color), (Layout::Grouped, Color::On));
        // pipe: flat, no colour
        let o = Output::resolve(false, false, None, false);
        assert_eq!((o.layout, o.color), (Layout::Flat, Color::Off));
        // tty + --flatten: flat but still coloured
        let o = Output::resolve(true, true, None, false);
        assert_eq!((o.layout, o.color), (Layout::Flat, Color::On));
        // NO_COLOR on a tty turns colour off; --color overrides it
        assert_eq!(Output::resolve(true, false, None, true).color, Color::Off);
        assert_eq!(
            Output::resolve(true, false, Some(true), true).color,
            Color::On
        );
        // --color on a pipe forces it on; --no-color on a tty forces it off
        assert_eq!(
            Output::resolve(false, false, Some(true), false).color,
            Color::On
        );
        assert_eq!(
            Output::resolve(true, false, Some(false), false).color,
            Color::Off
        );
    }

    #[test]
    fn every_match_span_is_highlighted() {
        let re = Regex::new("ab").unwrap();
        let o = Output {
            layout: Layout::Flat,
            color: Color::On,
        };
        let mut buf = Vec::new();
        o.line(&mut buf, &re, b"xab-ab-y").unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.matches("\x1b[1;31mab\x1b[0m").count(), 2, "{s:?}");
        assert!(s.starts_with('x') && s.ends_with("-y"));
    }
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
