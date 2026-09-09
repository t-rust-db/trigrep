// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Incremental index update (#34): diff the filesystem against the
//! `files` table, then apply every change as one journaled transaction.
//!
//! A file keeps its id for life, so a modified file only *adds* the
//! trigrams it did not have before; trigrams it lost stay in their
//! posting lists as false positives, and a deleted file's id stays in
//! its posting lists as a tombstone. Both are harmless for results —
//! the regex always runs on the real file, and an id without a `files`
//! row is skipped — and both cost only query time, which `--rebuild`
//! reclaims. The alternative (finding every posting list an id is in)
//! is a full scan of `trigrams` per changed file, which is exactly the
//! quadratic update the ticket warns against.
//!
//! Posting-list rewrites are batched per *touched trigram*: all pending
//! files' new ids are grouped by trigram in memory first, then each
//! posting list is read, merged and rewritten once, however many files
//! touched it.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::UNIX_EPOCH;

use db_storage::row::btree::{delete_row, insert_row, BtreeError, TableCursor};
use db_storage::row::record::{decode_record, encode_record, Value};

use crate::cache::{fnv1a64, Cache, Result, MAX_FILE_SIZE};
use crate::codec;

/// One `files` row.
#[derive(Debug, Clone)]
pub struct FileMeta {
    pub id: i64,
    pub path: String,
    pub mtime: i64,
    pub size: i64,
    pub hash: i64,
}

/// What an update did, for the `index` subcommand's one-line report.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub trigrams_rewritten: usize,
}

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::Text(s)) => s.to_string(),
        _ => String::new(),
    }
}

fn int(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Integer(i)) => *i,
        _ => 0,
    }
}

fn decode_file(rowid: i64, payload: &[u8], cache: &Cache) -> Result<FileMeta> {
    let v = decode_record(payload, cache.header.text_encoding)?;
    Ok(FileMeta {
        id: rowid,
        path: text(v.first()),
        mtime: int(v.get(1)),
        size: int(v.get(2)),
        hash: int(v.get(3)),
    })
}

fn encode_file(f: &FileMeta, cache: &Cache) -> Vec<u8> {
    encode_record(
        &[
            Value::Text(f.path.as_str().into()),
            Value::Integer(f.mtime),
            Value::Integer(f.size),
            Value::Integer(f.hash),
        ],
        cache.header.text_encoding,
    )
}

/// Every `files` row, in id order.
pub fn load_files(cache: &Cache) -> Result<Vec<FileMeta>> {
    let mut cursor = TableCursor::new(&cache.pager, &cache.header, cache.files_root);
    let mut out = Vec::new();
    let mut row = cursor.first_row()?;
    while let Some(r) = row {
        out.push(decode_file(r.rowid, &r.payload, cache)?);
        row = cursor.next_row()?;
    }
    Ok(out)
}

/// The `files` row for one id, or `None` for a tombstoned id.
pub fn lookup_file(cache: &Cache, id: i64) -> Result<Option<FileMeta>> {
    let mut cursor = TableCursor::new(&cache.pager, &cache.header, cache.files_root);
    match cursor.seek_row(id)? {
        Some(r) => Ok(Some(decode_file(r.rowid, &r.payload, cache)?)),
        None => Ok(None),
    }
}

/// The posting list for one packed trigram (empty if absent). The row
/// is a one-column record whose BLOB is the delta-varint list.
pub fn lookup_postings(cache: &Cache, trigram: i64) -> Result<Vec<i64>> {
    let mut cursor = TableCursor::new(&cache.pager, &cache.header, cache.trigrams_root);
    match cursor.seek_row(trigram)? {
        Some(r) => {
            let v = decode_record(&r.payload, cache.header.text_encoding)?;
            match v.first() {
                Some(Value::Blob(b)) => Ok(codec::decode_postings(b)?),
                _ => Err(format!("trigram row {trigram} has no BLOB column").into()),
            }
        }
        None => Ok(Vec::new()),
    }
}

fn encode_postings_row(ids: &[i64], cache: &Cache) -> Vec<u8> {
    encode_record(
        &[Value::Blob(codec::encode_postings(ids).into())],
        cache.header.text_encoding,
    )
}

/// Files with a NUL in their first 8 KiB are binary: not indexed, not
/// searched — grep's own heuristic.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

fn mtime_nanos(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

/// Content read for a file that will be (re)indexed, or `None` when it
/// is to be treated as absent (binary, oversized, or gone meanwhile).
fn read_indexable(path: &Path, size: u64) -> Option<Vec<u8>> {
    if size > MAX_FILE_SIZE {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if is_binary(&bytes) {
        return None;
    }
    Some(bytes)
}

/// Brings the cache for `root` up to date with the filesystem.
/// Chunk bounds for the write phase (#3). A build used to hold every
/// pending posting list until one commit at the end — 1.1 GB RSS on a
/// 1 GB tree. Now files are processed in windows and each window is its
/// own `Pager` transaction, so peak memory is one window's postings and a
/// kill leaves the cache at the last window boundary. Env overrides exist
/// so the crash test can force tiny windows.
pub const CHUNK_FILES_ENV: &str = "TRIGREP_CHUNK_FILES";
pub const CHUNK_BYTES_ENV: &str = "TRIGREP_CHUNK_BYTES";
const DEFAULT_CHUNK_FILES: usize = 4_000;
const DEFAULT_CHUNK_BYTES: usize = 256 << 20;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// Per-file work that does not touch the cache: stat, read, hash,
/// trigram extraction. Pure in the path, so it can run anywhere.
struct Scanned {
    rel: String,
    /// `None`: listed but unreadable/gone/binary/too large → treat as absent.
    body: Option<(i64, i64, i64, Option<Vec<i64>>)>, // (mtime, size, hash, trigrams if content must be indexed)
}

fn scan_one(root: &Path, entry: crate::walk::Entry, old: Option<&FileMeta>) -> Scanned {
    let rel = entry.rel;
    let full = root.join(&rel);
    let meta = match entry.metadata {
        Some(m) => m,
        None => match std::fs::metadata(&full) {
            Ok(m) => m,
            Err(_) => return Scanned { rel, body: None },
        },
    };
    if !meta.is_file() {
        return Scanned { rel, body: None };
    }
    let mtime = mtime_nanos(&meta);
    let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
    if let Some(old) = old {
        if old.mtime == mtime && old.size == size {
            // Unchanged by stat: keep the stored hash, no content read.
            return Scanned {
                rel,
                body: Some((mtime, size, old.hash, None)),
            };
        }
    }
    let Some(bytes) = read_indexable(&full, meta.len()) else {
        return Scanned { rel, body: None };
    };
    let hash = fnv1a64(&bytes) as i64;
    let trigrams = match old {
        Some(old) if old.hash == hash => None, // touched, same content
        _ => Some(codec::unique_trigrams(&bytes)),
    };
    Scanned {
        rel,
        body: Some((mtime, size, hash, trigrams)),
    }
}

/// One transaction: the window's file rows, the posting lists they touch,
/// and (on the first window) the in-progress marker.
fn commit_chunk(
    cache: &mut Cache,
    file_rows: &mut Vec<(Option<i64>, Option<FileMeta>)>,
    pending: &mut BTreeMap<i64, Vec<i64>>,
    stats: &mut Stats,
    first: &mut bool,
    last: bool,
) -> Result<()> {
    if file_rows.is_empty() && !last && !*first {
        pending.clear();
        return Ok(());
    }
    // Phase A: read every touched posting list (immutable borrow).
    let mut rewrites: Vec<(i64, bool, Vec<u8>)> = Vec::with_capacity(pending.len());
    for (trigram, ids) in std::mem::take(pending) {
        let mut ids = ids;
        ids.sort_unstable();
        ids.dedup();
        let mut current = lookup_postings(cache, trigram)?;
        let existed = !current.is_empty();
        if codec::merge_into(&mut current, &ids) {
            rewrites.push((trigram, existed, encode_postings_row(&current, cache)));
        }
    }
    // Phase B: the transaction, committed by `flush`.
    let header = cache.header;
    if *first {
        let marker = encode_record(
            &[Value::Text("incomplete".into()), Value::Text("1".into())],
            header.text_encoding,
        );
        ignore_missing(delete_row(
            &mut cache.pager,
            &header,
            cache.meta_root,
            crate::cache::META_INCOMPLETE_ROWID,
        ))?;
        insert_row(
            &mut cache.pager,
            &header,
            cache.meta_root,
            crate::cache::META_INCOMPLETE_ROWID,
            &marker,
        )?;
        *first = false;
    }
    for (delete_id, insert) in file_rows.drain(..) {
        if let Some(id) = delete_id {
            ignore_missing(delete_row(&mut cache.pager, &header, cache.files_root, id))?;
        }
        if let Some(f) = insert {
            let record = encode_file(&f, cache);
            insert_row(&mut cache.pager, &header, cache.files_root, f.id, &record)?;
        }
    }
    for (trigram, existed, blob) in &rewrites {
        if *existed {
            delete_row(&mut cache.pager, &header, cache.trigrams_root, *trigram)?;
        }
        insert_row(
            &mut cache.pager,
            &header,
            cache.trigrams_root,
            *trigram,
            blob,
        )?;
    }
    stats.trigrams_rewritten = stats.trigrams_rewritten.saturating_add(rewrites.len());
    if last {
        ignore_missing(delete_row(
            &mut cache.pager,
            &header,
            cache.meta_root,
            crate::cache::META_INCOMPLETE_ROWID,
        ))?;
    }
    cache.pager.flush()?;
    Ok(())
}

pub fn update(cache: &mut Cache, root: &Path) -> Result<Stats> {
    let mut stats = Stats::default();
    let existing: HashMap<String, FileMeta> = load_files(cache)?
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();
    let mut next_id = existing.values().map(|f| f.id).max().unwrap_or(0);
    let chunk_files = env_usize(CHUNK_FILES_ENV, DEFAULT_CHUNK_FILES);
    let chunk_bytes = env_usize(CHUNK_BYTES_ENV, DEFAULT_CHUNK_BYTES);

    let mut file_rows: Vec<(Option<i64>, Option<FileMeta>)> = Vec::new();
    let mut pending: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let mut pending_bytes = 0usize;
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut first = true;

    let present = crate::walk::list_files(root)?;
    let mut window: Vec<crate::walk::Entry> = Vec::with_capacity(chunk_files);
    let mut iter = present.into_iter().peekable();
    while iter.peek().is_some() {
        window.clear();
        while window.len() < chunk_files {
            match iter.next() {
                Some(e) => window.push(e),
                None => break,
            }
        }
        let scanned = scan_window(root, std::mem::take(&mut window), &existing);
        for sc in scanned {
            let old = existing.get(sc.rel.as_str());
            if let Some(old) = old {
                seen.insert(old.path.clone(), ());
            }
            let Some((mtime, size, hash, trigrams)) = sc.body else {
                if let Some(old) = old {
                    file_rows.push((Some(old.id), None));
                    stats.removed = stats.removed.saturating_add(1);
                }
                continue;
            };
            if let Some(old) = old {
                if old.mtime == mtime && old.size == size {
                    stats.unchanged = stats.unchanged.saturating_add(1);
                    continue;
                }
            }
            let (id, reindex) = match (old, &trigrams) {
                (Some(old), None) => (old.id, false),
                (Some(old), Some(_)) => (old.id, true),
                (None, _) => {
                    next_id = next_id.checked_add(1).ok_or("file id space exhausted")?;
                    (next_id, true)
                }
            };
            let row = FileMeta {
                id,
                path: sc.rel.clone(),
                mtime,
                size,
                hash,
            };
            file_rows.push((old.map(|o| o.id), Some(row)));
            match (old.is_some(), reindex) {
                (true, false) => stats.unchanged = stats.unchanged.saturating_add(1),
                (true, true) => stats.changed = stats.changed.saturating_add(1),
                (false, _) => stats.added = stats.added.saturating_add(1),
            }
            if let Some(ts) = trigrams {
                pending_bytes = pending_bytes.saturating_add(ts.len() * 8);
                for t in ts {
                    pending.entry(t).or_default().push(id);
                }
            }
            if file_rows.len() >= chunk_files || pending_bytes >= chunk_bytes {
                commit_chunk(
                    cache,
                    &mut file_rows,
                    &mut pending,
                    &mut stats,
                    &mut first,
                    false,
                )?;
                pending_bytes = 0;
            }
        }
    }
    for old in existing.values() {
        if !seen.contains_key(old.path.as_str()) {
            file_rows.push((Some(old.id), None));
            stats.removed = stats.removed.saturating_add(1);
        }
    }
    if file_rows.is_empty() && first {
        // Nothing changed and no window was committed: but a stale marker
        // from a killed run may still be there — clear it.
        let header = cache.header;
        let mut cur = TableCursor::new(&cache.pager, &header, cache.meta_root);
        if cur.seek_row(crate::cache::META_INCOMPLETE_ROWID)?.is_some() {
            delete_row(
                &mut cache.pager,
                &header,
                cache.meta_root,
                crate::cache::META_INCOMPLETE_ROWID,
            )?;
            cache.pager.flush()?;
        }
        return Ok(stats);
    }
    commit_chunk(
        cache,
        &mut file_rows,
        &mut pending,
        &mut stats,
        &mut first,
        true,
    )?;
    Ok(stats)
}

/// Reader threads for a window (#4). Reading, hashing and trigram
/// extraction are per-file and independent; the b-tree writes stay
/// single-threaded behind the `Pager`. Default `available_parallelism`
/// capped at 8; `TRIGREP_THREADS=1` is the sequential path.
pub const THREADS_ENV: &str = "TRIGREP_THREADS";

fn thread_count() -> usize {
    std::env::var(THREADS_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(8)
        })
}

/// Scans one window of entries with a small thread pool. Results come
/// back **in the window's order** regardless of which thread finished
/// first: each worker claims the next index and writes into that slot,
/// so id assignment (done by the caller, in path order) and the cache
/// file itself are byte-identical for any thread count.
fn scan_window(
    root: &Path,
    window: Vec<crate::walk::Entry>,
    existing: &HashMap<String, FileMeta>,
) -> Vec<Scanned> {
    let threads = thread_count().min(window.len().max(1));
    if threads <= 1 {
        return window
            .into_iter()
            .map(|e| {
                let old = existing.get(e.rel.as_str());
                scan_one(root, e, old)
            })
            .collect();
    }
    let n = window.len();
    let slots: Vec<std::sync::Mutex<Option<crate::walk::Entry>>> = window
        .into_iter()
        .map(|e| std::sync::Mutex::new(Some(e)))
        .collect();
    let out: Vec<std::sync::Mutex<Option<Scanned>>> =
        (0..n).map(|_| std::sync::Mutex::new(None)).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let entry = slots[i].lock().unwrap_or_else(|e| e.into_inner()).take();
                let Some(entry) = entry else { continue };
                let old = existing.get(entry.rel.as_str());
                let r = scan_one(root, entry, old);
                *out[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
            });
        }
    });
    out.into_iter()
        .map(|m| {
            m.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every slot is filled once its index was claimed")
        })
        .collect()
}

fn ignore_missing(r: std::result::Result<(), BtreeError>) -> Result<()> {
    match r {
        Ok(()) | Err(BtreeError::RowidNotFound { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
