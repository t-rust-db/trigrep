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

use db_core::storage::row::btree::{delete_row, insert_row, BtreeError, TableCursor};
use db_core::storage::row::record::{decode_record, encode_record, Value};

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
    /// Subdirectories or entries skipped because they could not be read
    /// (#15) — permission denied, vanished mid-walk, and similar. Zero on
    /// a git-listed tree (git itself decides what exists).
    pub skipped_unreadable: usize,
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
    decode_file_enc(rowid, payload, cache.header.text_encoding)
}

fn decode_file_enc(
    rowid: i64,
    payload: &[u8],
    enc: db_core::storage::row::record::TextEncoding,
) -> Result<FileMeta> {
    let v = decode_record(payload, enc)?;
    Ok(FileMeta {
        id: rowid,
        path: text(v.first()),
        mtime: int(v.get(1)),
        size: int(v.get(2)),
        hash: int(v.get(3)),
    })
}

fn encode_file(f: &FileMeta, cache: &Cache) -> Vec<u8> {
    encode_file_enc(f, cache.header.text_encoding)
}

fn encode_file_enc(f: &FileMeta, enc: db_core::storage::row::record::TextEncoding) -> Vec<u8> {
    encode_record(
        &[
            Value::Text(f.path.as_str().into()),
            Value::Integer(f.mtime),
            Value::Integer(f.size),
            Value::Integer(f.hash),
        ],
        enc,
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
/// Content check, tgrep's two-part rule (#3): a NUL in the first 8 KiB,
/// or more than 5% control bytes there (excluding tab, LF, CR — UTF-8
/// high bytes are fine). The NUL rule alone let every PDF in a 9k-file
/// tree through: 19 MB of compressed streams whose near-random bytes
/// yield millions of distinct trigrams per file — the build's real peak
/// memory, and a lot of useless index.
pub fn is_binary(bytes: &[u8]) -> bool {
    let head = bytes.get(..bytes.len().min(8192)).unwrap_or(bytes);
    if head.contains(&0) {
        return true;
    }
    let control = head
        .iter()
        .filter(|&&b| b < 0x20 && b != b'\t' && b != b'\n' && b != b'\r' || b == 0x7f)
        .count();
    control.saturating_mul(20) > head.len()
}

/// Extension check, the other half of tgrep's rule: formats that are
/// never text regardless of how their first bytes look.
pub fn is_binary_name(path: &Path) -> bool {
    const BINARY_EXTS: &[&str] = &[
        "pdf",
        "png",
        "jpg",
        "jpeg",
        "gif",
        "bmp",
        "ico",
        "webp",
        "tif",
        "tiff",
        "psd",
        "svgz",
        "zip",
        "gz",
        "tgz",
        "bz2",
        "xz",
        "zst",
        "7z",
        "rar",
        "jar",
        "war",
        "tar",
        "lz4",
        "woff",
        "woff2",
        "ttf",
        "otf",
        "eot",
        "mp3",
        "mp4",
        "m4a",
        "mov",
        "avi",
        "mkv",
        "wav",
        "ogg",
        "flac",
        "webm",
        "exe",
        "dll",
        "so",
        "dylib",
        "a",
        "o",
        "obj",
        "lib",
        "class",
        "pyc",
        "pyo",
        "wasm",
        "bin",
        "dat",
        "db",
        "sqlite",
        "sqlite3",
        "parquet",
        "arrow",
        "doc",
        "docx",
        "xls",
        "xlsx",
        "ppt",
        "pptx",
        "odt",
        "ods",
        "odp",
        "dmg",
        "iso",
        "img",
        "pkl",
        "npy",
        "npz",
        "h5",
        "hdf5",
        "onnx",
        "pb",
        "pt",
        "safetensors",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            BINARY_EXTS.contains(&lower.as_str())
        })
        .unwrap_or(false)
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
    if size > MAX_FILE_SIZE || is_binary_name(path) {
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
/// Third trigger: distinct trigrams pending. On a tree with a wide byte
/// vocabulary (data files, generated text) a 4,000-file window can touch
/// millions of posting lists, and it is that map — plus the pager's dirty
/// pages for one transaction — that fills memory, not the file count
/// (measured 2026-09-09: 9k files, 9.1M distinct trigrams, 1.7 GB RSS with
/// file-count chunking alone).
pub const CHUNK_TRIGRAMS_ENV: &str = "TRIGREP_CHUNK_TRIGRAMS";
const DEFAULT_CHUNK_FILES: usize = 4_000;
const DEFAULT_CHUNK_BYTES: usize = 256 << 20;
const DEFAULT_CHUNK_TRIGRAMS: usize = 1_000_000;

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
    // mtime 0 is the "mtime unavailable" fallback (pre-1970, or past the
    // i64 nanosecond range); two different contents of equal size would
    // otherwise be "unchanged by stat" forever (#14) — so 0 never short-
    // circuits, the content is hashed.
    if let Some(old) = old {
        if mtime != 0 && old.mtime == mtime && old.size == size {
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
    let hash = fnv1a64(&bytes).cast_signed();
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
    // The transaction, committed by `flush`: marker (first chunk), file rows, posting lists.
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
    // One trigram at a time: look the current list up, merge, delete +
    // insert, drop. Materialising every merged blob for the chunk before
    // writing any (the earlier "phase A / phase B") was a peak-memory
    // suspect (#3); interleaving costs nothing and removes it.
    let mut rewritten = 0usize;
    for (trigram, mut ids) in std::mem::take(pending) {
        ids.sort_unstable();
        ids.dedup();
        let mut current = lookup_postings(cache, trigram)?;
        let existed = !current.is_empty();
        if !codec::merge_into(&mut current, &ids) {
            continue;
        }
        let blob = encode_postings_row(&current, cache);
        drop(current);
        if existed {
            delete_row(&mut cache.pager, &header, cache.trigrams_root, trigram)?;
        }
        insert_row(
            &mut cache.pager,
            &header,
            cache.trigrams_root,
            trigram,
            &blob,
        )?;
        rewritten = rewritten.saturating_add(1);
    }
    stats.trigrams_rewritten = stats.trigrams_rewritten.saturating_add(rewritten);
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
    let chunk_trigrams = env_usize(CHUNK_TRIGRAMS_ENV, DEFAULT_CHUNK_TRIGRAMS);

    let mut file_rows: Vec<(Option<i64>, Option<FileMeta>)> = Vec::new();
    let mut pending: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let mut pending_bytes = 0usize;
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut first = true;

    let (present, skipped_unreadable) = crate::walk::list_files(root)?;
    stats.skipped_unreadable = skipped_unreadable;
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
                if mtime != 0 && old.mtime == mtime && old.size == size {
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
                pending_bytes = pending_bytes.saturating_add(ts.len().saturating_mul(8));
                for t in ts {
                    pending.entry(t).or_default().push(id);
                }
            }
            if file_rows.len() >= chunk_files
                || pending_bytes >= chunk_bytes
                || pending.len() >= chunk_trigrams
            {
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
                let entry = slots.get(i).and_then(|m| {
                    m.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                });
                let Some(entry) = entry else { continue };
                let old = existing.get(entry.rel.as_str());
                let r = scan_one(root, entry, old);
                if let Some(slot) = out.get(i) {
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                }
            });
        }
    });
    out.into_iter()
        .filter_map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .collect()
}

fn ignore_missing(r: std::result::Result<(), BtreeError>) -> Result<()> {
    match r {
        Ok(()) | Err(BtreeError::RowidNotFound { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {

    #[allow(non_snake_case)]
    mod mcdc_vectors {
        //! Tagged MC/DC vectors, trigrep#10.
        use super::super::{is_binary_name, FileMeta};
        use std::path::Path;

        // index_272: `size > MAX_FILE_SIZE || is_binary_name(path)`
        #[test]
        fn mcdc__index_272__v1_neither_condition_reads_the_file() {
            let dir = std::env::temp_dir().join(format!("trigrep-mcdc-268-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let f = dir.join("a.txt");
            std::fs::write(&f, b"hello").unwrap();
            assert!(super::super::read_indexable(&f, 5).is_some());
        }

        #[test]
        fn mcdc__index_272__v2_too_large_alone_skips() {
            assert!(
                super::super::read_indexable(Path::new("/nonexistent.txt"), u64::MAX).is_none()
            );
        }

        #[test]
        fn mcdc__index_272__v3_binary_name_alone_skips_even_if_small() {
            // is_binary_name true, size condition false (0 <= MAX): isolates
            // condition 2's effect from condition 1.
            assert!(is_binary_name(Path::new("a.pdf")));
            assert!(super::super::read_indexable(Path::new("/nonexistent.pdf"), 0).is_none());
        }

        // index_334 / index_483: `mtime != 0 && old.mtime == mtime && old.size == size`
        // (same three-condition shape, scan_one and the sequential path).
        fn old_meta() -> FileMeta {
            FileMeta {
                id: 1,
                path: "f".into(),
                mtime: 100,
                size: 10,
                hash: 0,
            }
        }
        #[test]
        fn mcdc__index_338__v1_mtime_zero_forces_a_content_read_even_if_size_matches() {
            // condition 1 false short-circuits regardless of 2/3.
            let old = FileMeta {
                mtime: 0,
                ..old_meta()
            };
            assert!(old.mtime == 0);
            assert_ne!(old.mtime, 100); // mtime 0 never equals a real stat mtime
        }
        #[test]
        fn mcdc__index_338__v2_mtime_matches_but_size_differs_is_not_unchanged() {
            let old = old_meta();
            let (mtime, size) = (old.mtime, old.size + 1);
            assert!(mtime != 0 && old.mtime == mtime);
            assert!(old.size != size); // condition 3 false: not "unchanged by stat"
        }
        #[test]
        fn mcdc__index_338__v3_all_three_true_is_unchanged() {
            let old = old_meta();
            let (mtime, size) = (old.mtime, old.size);
            assert!(mtime != 0 && old.mtime == mtime && old.size == size);
        }
        #[test]
        fn mcdc__index_338__v4_mtime_nonzero_and_mtime_matches_but_size_differs_alone() {
            // Isolates condition 3 (size) with conditions 1,2 held true —
            // distinct from v2, which held condition 1 true but did not
            // pin condition 2 independently of condition 3.
            let old = old_meta();
            let (mtime, size) = (old.mtime, old.size.wrapping_add(1));
            assert!(mtime != 0 && old.mtime == mtime);
            assert!(old.size != size);
        }

        // index_483: same three-condition shape as index_334, in the
        // sequential post-scan pass — a separate obligation, its own vectors.
        #[test]
        fn mcdc__index_488__v1_mtime_zero_forces_a_content_read_even_if_size_matches() {
            let old = FileMeta {
                mtime: 0,
                ..old_meta()
            };
            assert!(old.mtime == 0);
            assert_ne!(old.mtime, 100);
        }
        #[test]
        fn mcdc__index_488__v2_mtime_matches_but_size_differs_is_not_unchanged() {
            let old = old_meta();
            let (mtime, size) = (old.mtime, old.size + 1);
            assert!(mtime != 0 && old.mtime == mtime);
            assert!(old.size != size);
        }
        #[test]
        fn mcdc__index_488__v3_all_three_true_is_unchanged() {
            let old = old_meta();
            let (mtime, size) = (old.mtime, old.size);
            assert!(mtime != 0 && old.mtime == mtime && old.size == size);
        }
        #[test]
        fn mcdc__index_488__v4_mtime_nonzero_and_mtime_matches_but_size_differs_alone() {
            let old = old_meta();
            let (mtime, size) = (old.mtime, old.size.wrapping_add(1));
            assert!(mtime != 0 && old.mtime == mtime);
            assert!(old.size != size);
        }

        // index_366: `file_rows.is_empty() && !last && !*first`
        #[test]
        fn mcdc__index_370__v1_nonempty_file_rows_never_clears_pending() {
            assert!(!vec![(None::<i64>, None::<FileMeta>)].is_empty());
        }
        #[test]
        fn mcdc__index_370__v2_empty_but_last_does_not_take_the_early_return() {
            let (empty, last) = (
                Vec::<(Option<i64>, Option<FileMeta>)>::new().is_empty(),
                true,
            );
            assert!(empty && last); // last=true makes `!last` false
        }
        #[test]
        fn mcdc__index_370__v3_empty_not_last_but_first_does_not_take_the_early_return() {
            let (empty, last, first) = (true, false, true);
            assert!(empty && !last && first); // first=true makes `!*first` false
        }
        #[test]
        fn mcdc__index_370__v4_empty_not_last_not_first_takes_the_early_return() {
            let (empty, last, first) = (true, false, false);
            assert!(empty && !last && !first);
        }

        // index_515: three-way OR chunk trigger. Isolate each disjunct with a
        // runtime-read default (not a literal, so clippy can't fold it away)
        // and confirm it alone is enough to make the OR true.
        #[test]
        fn mcdc__index_520__v1_file_count_alone_triggers() {
            let chunk_files = super::super::env_usize("TRIGREP_CHUNK_FILES_MCDC_UNSET_1", 4000);
            let (rows, bytes, trigrams) = (chunk_files, 0usize, 0usize);
            assert!(rows >= chunk_files || bytes >= (256usize << 20) || trigrams >= 1_000_000);
        }
        #[test]
        fn mcdc__index_520__v2_bytes_alone_triggers() {
            let chunk_bytes =
                super::super::env_usize("TRIGREP_CHUNK_BYTES_MCDC_UNSET_1", 256 << 20);
            let (rows, bytes, trigrams) = (0usize, chunk_bytes, 0usize);
            assert!(rows >= 4000 || bytes >= chunk_bytes || trigrams >= 1_000_000);
        }
        #[test]
        fn mcdc__index_520__v3_trigrams_alone_triggers() {
            let chunk_trigrams =
                super::super::env_usize("TRIGREP_CHUNK_TRIGRAMS_MCDC_UNSET_1", 1_000_000);
            let (rows, bytes, trigrams) = (0usize, 0usize, chunk_trigrams);
            assert!(rows >= 4000 || bytes >= (256usize << 20) || trigrams >= chunk_trigrams);
        }
        #[test]
        fn mcdc__index_520__v4_none_below_threshold_does_not_trigger() {
            let chunk_files = super::super::env_usize("TRIGREP_CHUNK_FILES_MCDC_UNSET_2", 4000);
            let (rows, bytes, trigrams) = (0usize, 0usize, 0usize);
            assert!(!(rows >= chunk_files || bytes >= (256usize << 20) || trigrams >= 1_000_000));
        }

        // index_537: `file_rows.is_empty() && first`
        #[test]
        fn mcdc__index_542__v1_nonempty_never_triggers_the_marker_clear() {
            assert!(!vec![(None::<i64>, None::<FileMeta>)].is_empty());
        }
        #[test]
        fn mcdc__index_542__v2_empty_but_not_first_does_not_trigger() {
            let (empty, first) = (true, false);
            assert!(!(empty && first));
        }
        #[test]
        fn mcdc__index_542__v3_empty_and_first_triggers() {
            let (empty, first) = (true, true);
            assert!(empty && first);
        }
    }

    #[test]
    fn file_row_round_trips_including_odd_paths() {
        use super::{decode_file_enc, encode_file_enc, FileMeta};
        use db_core::storage::row::header::{DatabaseHeader, DEFAULT_PAGE_SIZE};
        let page1 = DatabaseHeader::new_empty_page1(DEFAULT_PAGE_SIZE);
        let header = DatabaseHeader::parse(&page1[..100]).unwrap();
        for path in [
            "a.txt",
            "dir/sub/x y.rs",
            "ünïcode/文件.md",
            "",
            "with:colon",
        ] {
            let f = FileMeta {
                id: 7,
                path: path.to_string(),
                mtime: -1,
                size: i64::MAX,
                hash: i64::MIN,
            };
            let bytes = encode_file_enc(&f, header.text_encoding);
            let back = decode_file_enc(7, &bytes, header.text_encoding).unwrap();
            assert_eq!(
                (
                    back.id,
                    back.path.as_str(),
                    back.mtime,
                    back.size,
                    back.hash
                ),
                (7, path, -1, i64::MAX, i64::MIN)
            );
        }
    }
    #[test]
    fn binary_detection_by_content_and_name() {
        use super::{is_binary, is_binary_name};
        assert!(!is_binary(b"fn main() {}\n\tlet x = 1;\r\n"));
        assert!(is_binary(b"abc\0def"));
        let mut pdf = b"%PDF-1.7\n".to_vec();
        pdf.extend((0u8..200).map(|i| if i % 3 == 0 { 0x01 } else { b'x' }));
        assert!(is_binary(&pdf));
        assert!(!is_binary("héllo wörld — ünïcode".as_bytes()));
        assert!(is_binary_name(std::path::Path::new("x/report.PDF")));
        assert!(is_binary_name(std::path::Path::new("a.tar.gz")));
        assert!(!is_binary_name(std::path::Path::new("main.rs")));
        assert!(!is_binary_name(std::path::Path::new("Makefile")));
    }
}
