//! shoal-journal — the persistent command journal and content-addressed output store.
//!
//! Implements TDD §9: every executed statement becomes an `entry` row in a SQLite
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

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

mod cas;
mod gc;
mod query;
mod schema;
#[cfg(test)]
mod tests;
mod transcript;
mod undo;

pub use cas::{JournalOptions, OutputMeta, OutputRow};
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
        let conn = Connection::open(state_dir.join("journal.db"))?;
        // `PRAGMA journal_mode=WAL` returns a result row; consume it.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Wait (rather than immediately fail with SQLITE_BUSY) for a competing
        // writer's lock: the journal is shared across processes and every
        // journaling call site swallows errors, so a busy failure here silently
        // drops the entry and its undo inverse. See `JournalOptions::busy_timeout`.
        conn.busy_timeout(options.busy_timeout)?;
        Self::init_schema(&conn)?;
        Ok(Journal {
            conn,
            cas_root,
            _cas_tempdir: None,
            output_hard_cap: options.output_hard_cap,
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
        Self::init_schema(&conn)?;
        Ok(Journal {
            conn,
            cas_root,
            _cas_tempdir: Some(tempdir),
            output_hard_cap: options.output_hard_cap,
        })
    }
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
