//! shoal-journal — the persistent command journal and content-addressed output store.
//!
//! Implements site/content/internals/language-conformance-contract.md: every executed statement becomes an `entry` row in a SQLite
//! database (WAL mode), its captured outputs are stored compressed (zstd) in a
//! blake3-keyed content-addressed store (CAS) on disk, undo inverses are recorded
//! per entry, and pins protect blobs from garbage collection.
//!
//! # Storage layout
//!
//! ```text
//! <state_dir>/journal.db                              SQLite database (WAL)
//! <state_dir>/cas/<hex[0..2]>/<hex[2..4]>/<hex>.zst   zstd-compressed blobs
//! ```
//!
//! # Concurrency
//!
//! A [`Journal`] is a single-handle, single-thread object (`Send` but not `Sync`,
//! courtesy of the underlying `rusqlite::Connection`). Each write is a single
//! SQLite statement and therefore atomic; the WAL journal makes an unfinished
//! entry (appended but never [`Journal::finish`]ed, e.g. across a crash) durable
//! and visible with `NULL` status on reopen.
//!
//! # Module layout
//!
//! This file holds only the [`Journal`] handle itself (construction) and a
//! handful of low-level helpers shared by every other module. Each area of
//! functionality lives in its own file, all as further `impl Journal { .. }`
//! blocks (the same multi-file-impl pattern `shoal-eval`'s `reef.rs` uses):
//!
//! - [`schema`] — table/index DDL, and the `entry` row lifecycle (`append`/`finish`).
//! - [`cas`] — the blake3+zstd content-addressed blob store (`record_output`/`read_blob`).
//! - [`undo`] — typed undo inverses: recording and TOCTOU-safe replay.
//! - [`gc`] — pins and blob garbage collection.
//! - [`query`] — filtered, newest-first entry queries with joined outputs, plus the targeted
//!   `entries_by_id` fetch.
//! - [`transcript`] — durable `session.transcript` channel event payloads, keyed by `entry_id`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode};

mod cas;
mod gc;
mod query;
mod schema;
#[cfg(test)]
mod tests;
mod transcript;
mod undo;

pub use cas::{Cas, JournalOptions, OutputMeta, OutputRow};
pub use gc::{GcBlob, GcOptions, GcReport};
pub use query::{EntryRow, JournalQuery};
pub use schema::EntryRecord;
pub use transcript::TranscriptEventRow;
pub use undo::{FileFingerprint, UndoError, UndoInverse, UndoReport, UndoStatus, UndoStep};

/// Handle to a journal: a SQLite database plus an on-disk CAS directory.
///
/// Obtain one with [`Journal::open`] (persistent) or [`Journal::in_memory`]
/// (throwaway: in-memory database, temp-dir CAS that is deleted on drop).
pub struct Journal {
    conn: Connection,
    cas_root: PathBuf,
    /// Keeps the CAS temp dir alive for the lifetime of an in-memory journal.
    _cas_tempdir: Option<tempfile::TempDir>,
    output_hard_cap: usize,
    blob_page_cache: RefCell<BlobPageCache>,
}

const BLOB_PAGE_CACHE_MAX_BYTES: usize = 1024 * 1024;
const BLOB_PAGE_CACHE_MAX_ENTRIES: usize = 256;

struct BlobPageCacheEntry {
    hash: String,
    offset: u64,
    length: usize,
    total: u64,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct BlobPageCache {
    entries: VecDeque<BlobPageCacheEntry>,
    bytes: usize,
}

impl BlobPageCache {
    fn get(&mut self, hash: &str, offset: u64, length: usize) -> Option<(u64, Vec<u8>)> {
        let index = self.entries.iter().position(|entry| {
            entry.hash == hash && entry.offset == offset && entry.length == length
        })?;
        let entry = self.entries.remove(index)?;
        let result = (entry.total, entry.bytes.clone());
        self.entries.push_back(entry);
        Some(result)
    }

    fn insert(&mut self, entry: BlobPageCacheEntry) {
        if entry.bytes.len() > BLOB_PAGE_CACHE_MAX_BYTES {
            return;
        }
        if let Some(index) = self.entries.iter().position(|existing| {
            existing.hash == entry.hash
                && existing.offset == entry.offset
                && existing.length == entry.length
        }) && let Some(replaced) = self.entries.remove(index)
        {
            self.bytes = self.bytes.saturating_sub(replaced.bytes.len());
        }
        self.bytes = self.bytes.saturating_add(entry.bytes.len());
        self.entries.push_back(entry);
        while self.bytes > BLOB_PAGE_CACHE_MAX_BYTES
            || self.entries.len() > BLOB_PAGE_CACHE_MAX_ENTRIES
        {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(evicted.bytes.len());
        }
    }
}

impl Journal {
    /// Open (creating if needed) the journal under `state_dir`.
    ///
    /// Creates the directory tree, `<state_dir>/journal.db` in WAL mode, and
    /// `<state_dir>/cas/`.
    pub fn open(state_dir: &Path) -> rusqlite::Result<Journal> {
        Self::open_with_options(state_dir, JournalOptions::default())
    }

    pub fn open_with_options(
        state_dir: &Path,
        options: JournalOptions,
    ) -> rusqlite::Result<Journal> {
        let cas_root = state_dir.join("cas");
        fs::create_dir_all(&cas_root).map_err(io_to_sql)?;
        let db_path = state_dir.join("journal.db");
        let started = Instant::now();
        let conn = loop {
            let attempt = (|| {
                let conn = Connection::open(&db_path)?;
                // Install contention handling before *any* pragma or schema
                // write. `journal_mode=WAL` itself takes a database lock.
                conn.busy_timeout(options.busy_timeout.saturating_sub(started.elapsed()))?;
                // `PRAGMA journal_mode=WAL` returns a result row; consume it.
                conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
                conn.pragma_update(None, "synchronous", "NORMAL")?;
                Self::migrate(&conn)?;
                Ok(conn)
            })();
            match attempt {
                Ok(conn) => break conn,
                Err(error)
                    if is_sqlite_contention(&error)
                        && !options.busy_timeout.is_zero()
                        && started.elapsed() < options.busy_timeout =>
                {
                    // SQLite's busy handler covers SQLITE_BUSY while waiting
                    // on a lock, but concurrent first-open DDL/journal-mode
                    // transitions can also return SQLITE_LOCKED immediately.
                    // Reopen and retry within the same bounded policy window.
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error),
            }
        };
        Ok(Journal {
            conn,
            cas_root,
            _cas_tempdir: None,
            output_hard_cap: options.output_hard_cap,
            blob_page_cache: RefCell::default(),
        })
    }

    /// Open a throwaway journal: in-memory SQLite database, CAS in a fresh
    /// temporary directory that lives exactly as long as the returned `Journal`.
    pub fn in_memory() -> rusqlite::Result<Journal> {
        Self::in_memory_with_options(JournalOptions::default())
    }

    pub fn in_memory_with_options(options: JournalOptions) -> rusqlite::Result<Journal> {
        let tempdir = tempfile::tempdir().map_err(io_to_sql)?;
        let cas_root = tempdir.path().join("cas");
        fs::create_dir_all(&cas_root).map_err(io_to_sql)?;
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(options.busy_timeout)?;
        Self::migrate(&conn)?;
        Ok(Journal {
            conn,
            cas_root,
            _cas_tempdir: Some(tempdir),
            output_hard_cap: options.output_hard_cap,
            blob_page_cache: RefCell::default(),
        })
    }
}

fn is_sqlite_contention(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(code.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// rusqlite has no dedicated I/O error variant; `ToSqlConversionFailure` is the
/// conventional carrier for an arbitrary boxed error crossing a `rusqlite::Result`
/// boundary (CAS file I/O, zstd, temp-dir creation).
fn io_to_sql(e: io::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

/// Lowercase hex encoding of a byte slice.
fn hex_string(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing into a String cannot fail.
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

fn hex_bytes(hex: &str) -> Result<Vec<u8>, ()> {
    if !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| ()))
        .collect()
}
