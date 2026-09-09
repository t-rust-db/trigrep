// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The per-root cache file (#34): a real SQLite-format database under
//! the XDG cache directory, opened through `db-storage`'s `Pager` so
//! commits are journaled and a killed writer is rolled back on the next
//! open — the same crash story `sqlite3` itself has, for free.
//!
//! Three rowid tables, created once through the b-tree layer directly
//! (no SQL ever runs), each registered in `sqlite_master` with real DDL
//! text so `sqlite3 <cache>.db .schema` shows exactly what is in there:
//!
//! - `meta(key TEXT, value TEXT)` — rowid 1 holds the canonical root path.
//! - `files(path TEXT, mtime INTEGER, size INTEGER, hash INTEGER)` —
//!   rowid is the file id the posting lists refer to.
//! - `trigrams(postings BLOB)` — rowid is the packed trigram
//!   ([`crate::codec::pack`]), payload a delta-varint posting list.

use std::error::Error;
use std::path::{Path, PathBuf};

use db_storage::row::btree::{
    bump_schema_cookie, create_empty_table_root, insert_master_row, insert_row, MasterEntry,
    TableCursor,
};
use db_storage::row::header::{DatabaseHeader, DEFAULT_PAGE_SIZE};
use db_storage::row::pager::Pager;
use db_storage::row::record::{encode_record, Value};
use db_storage::row::schema::read_schema;
use db_storage::row::vfs::{UnixVfs, Vfs};

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Opens an existing cache file: parse the 100-byte header from disk, then
/// open the `Pager` at that page size (hot-journal recovery happens in
/// `Pager::open`). The cache is always written by this pager, so the
/// WAL-fallback path `sqlite-rs`'s `dump::open` carries for foreign files is
/// not needed here.
pub fn open_db<V: Vfs + Clone + 'static>(vfs: &V, path: &Path) -> Result<(DatabaseHeader, Pager)> {
    let file = vfs.open_read(path)?;
    let mut buf = [0u8; db_storage::row::header::HEADER_LEN];
    file.read_at(&mut buf, 0)?;
    let header = DatabaseHeader::parse(&buf)?;
    let pager = Pager::open(vfs, path, header.page_size)?;
    Ok((header, pager))
}

/// Environment override for the cache directory; tests use it so they
/// never touch the real `~/.cache`, and so can a caller who wants the
/// cache on a different disk.
pub const CACHE_DIR_ENV: &str = "TRIGREP_CACHE_DIR";

/// Maximum file size indexed; larger files are skipped (a generated
/// bundle or a data dump is rarely what a grep is after, and their
/// trigram set is a poor filter anyway).
pub const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

const FILES_SQL: &str =
    "CREATE TABLE files(path TEXT NOT NULL, mtime INTEGER NOT NULL, size INTEGER NOT NULL, hash INTEGER NOT NULL)";
const TRIGRAMS_SQL: &str = "CREATE TABLE trigrams(postings BLOB NOT NULL)";
const META_SQL: &str = "CREATE TABLE meta(key TEXT NOT NULL, value TEXT NOT NULL)";

/// An open cache with its three root pages resolved.
pub struct Cache {
    pub path: PathBuf,
    pub header: DatabaseHeader,
    pub pager: Pager,
    pub files_root: u32,
    pub trigrams_root: u32,
    pub meta_root: u32,
}

/// `meta` rowid of the "an update is in progress" marker (#3). Present
/// between the first chunk commit of an update and its last; a cache that
/// still has it on open was killed mid-update and must be brought up to
/// date before it is searched, even without `-u`.
pub const META_INCOMPLETE_ROWID: i64 = 2;

/// 64-bit FNV-1a: the content hash in `files` and the cache-file name.
/// Not cryptographic and not meant to be — it only has to notice that
/// bytes changed under an unchanged mtime/size.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Where the cache for `root` (already canonicalized) lives.
pub fn cache_path(root: &Path) -> Result<PathBuf> {
    let dir = match std::env::var_os(CACHE_DIR_ENV) {
        Some(d) => PathBuf::from(d),
        None => dirs::cache_dir()
            .ok_or("no cache directory for this platform; set TRIGREP_CACHE_DIR")?
            .join("trigrep"),
    };
    let key = fnv1a64(root.as_os_str().as_encoded_bytes());
    Ok(dir.join(format!("{key:016x}.db")))
}

/// Opens (creating and bootstrapping if needed) the cache for `root`.
/// `rebuild` deletes any existing cache first. Returns whether this call
/// just bootstrapped an empty schema — i.e. there was nothing to search
/// yet — which callers use to decide whether a scan is unavoidable (#38:
/// the default search path only ever scans when there is nothing cached
/// at all; an already-populated cache is searched as-is unless the
/// caller explicitly asks for a refresh).
pub fn open(root: &Path, rebuild: bool) -> Result<(Cache, bool)> {
    let path = cache_path(root)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if rebuild {
        remove_if_exists(&path)?;
        let mut journal = path.as_os_str().to_owned();
        journal.push("-journal");
        remove_if_exists(Path::new(&journal))?;
    }
    // Absent, or present but empty (SQLite's own rule: a zero-byte file
    // is a valid empty database, #14): write the empty page-1 image so
    // the pager has a header to open. Guarded by an exclusive lock,
    // re-checked once held (#15): two `tg` processes racing the very
    // first index of a root both reach this line with the file absent
    // or empty; without the lock they can both create/write it and one
    // process's page-1 write can land after the other's, torn. The lock
    // is released before `open_db` takes the pager's own (fresh, shared)
    // lock on a *different* file handle, so there is no self-deadlock —
    // flock is per open-file-description, not per path.
    {
        let file = UnixVfs.create_or_open_write(&path)?;
        let mut lock = file.lock_shared()?;
        // The exclusive escalation is non-blocking (matching SQLite's own
        // `os_unix.c`: `F_SETLK`, not `F_SETLKW`) — a losing racer gets
        // `VfsError::Locked` immediately, not a wait. Retry with a short
        // backoff, SQLite's own busy-handler convention, rather than
        // surface a spurious "database is locked" for what is really
        // "someone else is two milliseconds into writing eight bytes".
        let mut attempt: u32 = 0;
        loop {
            match lock.escalate_to_exclusive() {
                Ok(()) => break,
                Err(_) if attempt < 200 => {
                    attempt = attempt.saturating_add(1);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(e.into()),
            }
        }
        if file.size()? == 0 {
            file.write_at(&DatabaseHeader::new_empty_page1(DEFAULT_PAGE_SIZE), 0)?;
            file.sync()?;
        }
        lock.de_escalate_to_shared()?;
    }

    let (header, mut pager) = open_db(&UnixVfs, &path)?;
    let roots = read_roots(&pager, &header)?;
    let (files_root, trigrams_root, meta_root, mut fresh) = match roots {
        Some((f, t, m)) => (f, t, m, false),
        None => {
            bootstrap(&mut pager, &header, root)?;
            let (f, t, m) =
                read_roots(&pager, &header)?.ok_or("cache bootstrap left no schema behind")?;
            (f, t, m, true)
        }
    };
    if !fresh {
        // A killed chunked update (#3) leaves the marker behind: treat the
        // cache as needing its first scan, never search it as it stands.
        let mut cur = TableCursor::new(&pager, &header, meta_root);
        fresh = cur.seek_row(META_INCOMPLETE_ROWID)?.is_some();

        // #15: the cache is addressed by a 64-bit hash of the root path,
        // and a copied cache directory (or, astronomically unlikely, a
        // real collision) would otherwise silently serve one tree's
        // index for another's search — wrong results, no error. The
        // `meta` row written at bootstrap names the root it was built
        // for; verify it, and rebuild rather than trust a mismatch.
        if let Some(stored) = read_stored_root(&pager, &header, meta_root)? {
            if stored != root.to_string_lossy() {
                drop(pager);
                return open(root, true);
            }
        }
    }
    Ok((
        Cache {
            path,
            header,
            pager,
            files_root,
            trigrams_root,
            meta_root,
        },
        fresh,
    ))
}

/// The root path stored in `meta` at bootstrap (rowid 1), or `None` if
/// that row is somehow absent (a cache this old code never wrote).
fn read_stored_root(
    pager: &Pager,
    header: &DatabaseHeader,
    meta_root: u32,
) -> Result<Option<String>> {
    let mut cursor = TableCursor::new(pager, header, meta_root);
    let Some(row) = cursor.seek_row(1)? else {
        return Ok(None);
    };
    let record = db_storage::row::record::decode_record(&row.payload, header.text_encoding)?;
    match record.get(1) {
        Some(db_storage::row::record::Value::Text(t)) => Ok(Some(t.to_string())),
        _ => Ok(None),
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// `(files, trigrams)` root pages (`meta` is checked but never read back), or `None` for a fresh file.
/// A file with *some* of the tables is neither — it is a foreign or
/// damaged database and refusing beats silently indexing into it.
fn read_roots(pager: &Pager, header: &DatabaseHeader) -> Result<Option<(u32, u32, u32)>> {
    let mut cursor = TableCursor::new(pager, header, 1);
    let schemas = read_schema(&mut cursor, header.text_encoding)?;
    if schemas.is_empty() {
        return Ok(None);
    }
    let root_of = |name: &str| -> Result<u32> {
        schemas
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.root_page)
            .ok_or_else(|| format!("not a trigrep cache: table {name:?} missing").into())
    };
    Ok(Some((
        root_of("files")?,
        root_of("trigrams")?,
        root_of("meta")?,
    )))
}

/// One transaction: three empty table roots, their `sqlite_master`
/// rows, the schema-cookie bump, and the `meta` row naming the root.
fn bootstrap(pager: &mut Pager, header: &DatabaseHeader, root: &Path) -> Result<()> {
    let files_root = create_empty_table_root(pager)?;
    let trigrams_root = create_empty_table_root(pager)?;
    let meta_root = create_empty_table_root(pager)?;
    for (name, rootpage, sql) in [
        ("files", files_root, FILES_SQL),
        ("trigrams", trigrams_root, TRIGRAMS_SQL),
        ("meta", meta_root, META_SQL),
    ] {
        insert_master_row(
            pager,
            header,
            &MasterEntry {
                kind: "table".to_string(),
                name: name.to_string(),
                tbl_name: name.to_string(),
                rootpage,
                sql: sql.to_string(),
            },
        )?;
    }
    bump_schema_cookie(pager)?;
    let record = encode_record(
        &[
            Value::Text("root".into()),
            Value::Text(root.to_string_lossy().as_ref().into()),
        ],
        header.text_encoding,
    );
    insert_row(pager, header, meta_root, 1, &record)?;
    pager.flush()?;
    Ok(())
}
