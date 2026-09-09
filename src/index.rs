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
pub fn update(cache: &mut Cache, root: &Path) -> Result<Stats> {
    let mut stats = Stats::default();
    let existing: HashMap<String, FileMeta> = load_files(cache)?
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();
    let mut next_id = existing.values().map(|f| f.id).max().unwrap_or(0);

    // Pending writes, all applied below in one transaction.
    let mut file_rows: Vec<(Option<i64>, Option<FileMeta>)> = Vec::new(); // (delete id, insert)
    let mut pending: BTreeMap<i64, Vec<i64>> = BTreeMap::new(); // trigram -> new ids
    let mut seen: HashMap<&str, ()> = HashMap::new();

    let present = crate::walk::list_files(root)?;
    for entry in present {
        let rel = entry.rel;
        let full = root.join(&rel);
        // Reuse the walk's own `stat` when it already did one (the
        // non-git fallback) instead of paying for a second one here
        // (#38); a git-sourced entry has none yet, so this is the only
        // stat that path pays.
        let meta = match entry.metadata {
            Some(m) => m,
            None => {
                let Ok(m) = std::fs::metadata(&full) else {
                    continue; // listed (e.g. by git) but gone: same as absent
                };
                m
            }
        };
        if !meta.is_file() {
            continue;
        }
        let old = existing.get(rel.as_str());
        if let Some(old) = old {
            seen.insert(old.path.as_str(), ());
        }
        let mtime = mtime_nanos(&meta);
        let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
        if let Some(old) = old {
            if old.mtime == mtime && old.size == size {
                stats.unchanged = stats.unchanged.saturating_add(1);
                continue;
            }
        }
        let Some(bytes) = read_indexable(&full, meta.len()) else {
            if let Some(old) = old {
                file_rows.push((Some(old.id), None));
                stats.removed = stats.removed.saturating_add(1);
            }
            continue;
        };
        let hash = fnv1a64(&bytes) as i64;
        let (id, reindex) = match old {
            Some(old) if old.hash == hash => (old.id, false),
            Some(old) => (old.id, true),
            None => {
                next_id = next_id.checked_add(1).ok_or("file id space exhausted")?;
                (next_id, true)
            }
        };
        let row = FileMeta {
            id,
            path: rel.clone(),
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
        if reindex {
            for t in codec::unique_trigrams(&bytes) {
                pending.entry(t).or_default().push(id);
            }
        }
    }
    for old in existing.values() {
        if !seen.contains_key(old.path.as_str()) {
            file_rows.push((Some(old.id), None));
            stats.removed = stats.removed.saturating_add(1);
        }
    }

    if file_rows.is_empty() {
        return Ok(stats);
    }

    // Phase A: read every touched posting list (immutable borrow).
    let mut rewrites: Vec<(i64, bool, Vec<u8>)> = Vec::with_capacity(pending.len());
    for (trigram, mut ids) in pending {
        ids.sort_unstable();
        ids.dedup();
        let mut current = lookup_postings(cache, trigram)?;
        let existed = !current.is_empty();
        if codec::merge_into(&mut current, &ids) {
            rewrites.push((trigram, existed, encode_postings_row(&current, cache)));
        }
    }

    // Phase B: one transaction, committed by `flush` (rolled back on the
    // next open if we die before it completes).
    let header = cache.header;
    for (delete_id, insert) in &file_rows {
        if let Some(id) = delete_id {
            ignore_missing(delete_row(&mut cache.pager, &header, cache.files_root, *id))?;
        }
        if let Some(f) = insert {
            let record = encode_file(f, cache);
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
    stats.trigrams_rewritten = rewrites.len();
    cache.pager.flush()?;
    Ok(stats)
}

fn ignore_missing(r: std::result::Result<(), BtreeError>) -> Result<()> {
    match r {
        Ok(()) | Err(BtreeError::RowidNotFound { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
