//! The `entry` table: schema creation plus the append/finish lifecycle.
//!
//! An entry is recorded at execution start ([`Journal::append`]) with its
//! completion columns (`status`/`ok`/`dur_ns`) `NULL`, then filled in by
//! [`Journal::finish`] once the statement has run. The WAL journal mode
//! (set up in [`Journal::open_with_options`]) makes an entry that was
//! appended but never finished (e.g. a crash mid-execution) durable and
//! visible with `NULL` status on reopen.

use rusqlite::{Connection, params};

use crate::Journal;

/// A journal entry as recorded at execution start ([`Journal::append`]).
///
/// Status, success, and duration are unknown at this point; they are filled in
/// later by [`Journal::finish`].
#[derive(Debug, Clone)]
pub struct EntryRecord {
    /// Session identifier the statement ran in.
    pub session: String,
    /// Acting principal: `"human"` or `"agent:<name>"`.
    pub principal: String,
    /// Wall-clock start timestamp, nanoseconds since the Unix epoch.
    pub ts_ns: i64,
    /// Bytes of the working directory path (paths are bytes-backed, TDD §13.1).
    pub cwd: Vec<u8>,
    /// Source text exactly as typed.
    pub src: String,
    /// Canonical AST as JSON.
    pub ast_json: String,
    /// JSON array of effect instances; `"[\"opaque\"]"` for T0 commands.
    pub effects_json: String,
    /// Whether the effects are opaque (T0 / `sh { }`).
    pub opaque: bool,
}

impl Journal {
    /// Create tables and indexes (idempotent). Schema per TDD §9.
    pub(crate) fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS entry(
                 id        INTEGER PRIMARY KEY,
                 session   TEXT    NOT NULL,
                 principal TEXT    NOT NULL,
                 ts        INTEGER NOT NULL,
                 dur_ns    INTEGER,
                 cwd       BLOB    NOT NULL,
                 env_hash  BLOB,
                 src       TEXT    NOT NULL,
                 ast       BLOB    NOT NULL,
                 effects   TEXT    NOT NULL,
                 status    INTEGER,
                 ok        BOOL,
                 opaque    BOOL    NOT NULL
             );
             CREATE TABLE IF NOT EXISTS output(
                 entry_id INTEGER NOT NULL,
                 kind     TEXT    NOT NULL,
                 hash     BLOB    NOT NULL,
                 len      INTEGER NOT NULL,
                 meta     TEXT
             );
             CREATE TABLE IF NOT EXISTS undo(
                 entry_id INTEGER NOT NULL,
                 op       TEXT    NOT NULL,
                 inverse  TEXT    NOT NULL
             );
             CREATE TABLE IF NOT EXISTS pin(
                 hash BLOB PRIMARY KEY
             );
             CREATE TABLE IF NOT EXISTS blob(
                 hash BLOB PRIMARY KEY,
                 stored_len INTEGER NOT NULL,
                 created_ns INTEGER NOT NULL,
                 last_access_ns INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_entry_ts     ON entry(ts);
             CREATE INDEX IF NOT EXISTS idx_output_entry ON output(entry_id);
             CREATE INDEX IF NOT EXISTS idx_undo_entry   ON undo(entry_id);",
        )
    }

    /// Record an entry at execution start. Status, success verdict, and duration
    /// are `NULL` until [`Journal::finish`]. Returns the new entry id.
    pub fn append(&self, e: &EntryRecord) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO entry (session, principal, ts, dur_ns, cwd, env_hash, src, ast, effects,
                                status, ok, opaque)
             VALUES (?1, ?2, ?3, NULL, ?4, NULL, ?5, ?6, ?7, NULL, NULL, ?8)",
            params![
                e.session,
                e.principal,
                e.ts_ns,
                e.cwd,
                e.src,
                e.ast_json,
                e.effects_json,
                e.opaque
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Fill in the completion columns of a previously appended entry.
    ///
    /// `status` is `None` for signal deaths (TDD §13.6: never 128+n encoded).
    /// Errors with `StatementChangedRows(0)` if `id` does not exist.
    pub fn finish(
        &self,
        id: i64,
        status: Option<i32>,
        ok: bool,
        dur_ns: i64,
    ) -> rusqlite::Result<()> {
        let changed = self.conn.execute(
            "UPDATE entry SET status = ?1, ok = ?2, dur_ns = ?3 WHERE id = ?4",
            params![status, ok, dur_ns, id],
        )?;
        if changed == 0 {
            return Err(rusqlite::Error::StatementChangedRows(0));
        }
        Ok(())
    }
}
