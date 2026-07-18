//! The `entry` table: schema creation plus the append/finish lifecycle.
//!
//! An entry is recorded at execution start ([`Journal::append`]) with its
//! completion columns (`status`/`ok`/`dur_ns`) `NULL`, then filled in by
//! [`Journal::finish`] once the statement has run. The WAL journal mode
//! (set up in [`Journal::open_with_options`]) makes an entry that was
//! appended but never finished (e.g. a crash mid-execution) durable and
//! visible with `NULL` status on reopen.
//!
//! # Schema versioning
//!
//! [`Journal::migrate`] uses SQLite's built-in `PRAGMA user_version`. Version 2 added explicit
//! entry kinds and parent-execution links; earlier builds inferred coarse kernel entries from the
//! serialized AST shape, which made durable event queries depend on an incidental JSON layout.

use std::io;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::{Connection, ToSql, params};

use crate::storage::DB_WRITE_RESERVE_BYTES;
use crate::{Journal, io_to_sql};

/// The schema version this build of `shoal-journal` understands, stamped into
/// `PRAGMA user_version` by [`Journal::migrate`] on every open.
///
/// Bump this when existing tables need new columns or changed semantics. A new independent
/// `CREATE TABLE IF NOT EXISTS` may remain an idempotent unversioned addition when old readers do
/// not need to understand it.
pub(crate) const CURRENT_SCHEMA_VERSION: i64 = 2;

/// Durable semantic role of a journal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// One top-level Shoal statement recorded by the evaluator.
    Statement,
    /// One whole kernel execution, potentially owning several statement rows.
    Exec,
    /// One completed approval grant audit record.
    Approval,
}

impl EntryKind {
    /// Stable SQLite/wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Statement => "statement",
            Self::Exec => "exec",
            Self::Approval => "approval",
        }
    }
}

impl std::fmt::Display for EntryKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for EntryKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "statement" => Ok(Self::Statement),
            "exec" => Ok(Self::Exec),
            "approval" => Ok(Self::Approval),
            _ => Err(format!("unknown journal entry kind {value:?}")),
        }
    }
}

impl ToSql for EntryKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(
            self.as_str().as_bytes(),
        )))
    }
}

impl FromSql for EntryKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let value = value.as_str()?;
        value
            .parse()
            .map_err(|message: String| FromSqlError::Other(message.into()))
    }
}

/// A journal entry as recorded at execution start ([`Journal::append`]).
///
/// Status, success, and duration are unknown at this point; they are filled in
/// later by [`Journal::finish`].
#[derive(Debug, Clone)]
pub struct EntryRecord {
    /// Semantic role of this row.
    pub kind: EntryKind,
    /// Coarse execution row that owns this entry, when applicable.
    pub parent_id: Option<i64>,
    /// Session identifier the statement ran in.
    pub session: String,
    /// Acting principal: `"human"` or `"agent:<name>"`.
    pub principal: String,
    /// Wall-clock start timestamp, nanoseconds since the Unix epoch.
    pub ts_ns: i64,
    /// Bytes of the working directory path (paths are bytes-backed, site/content/internals/language-conformance-contract.md).
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
    /// Ensure `conn`'s on-disk schema exists and is stamped at [`CURRENT_SCHEMA_VERSION`].
    ///
    /// Called by every constructor that opens a connection (`open_with_options`,
    /// `in_memory_with_options`) in place of a bare [`Journal::init_schema`] call. SQLite's
    /// `PRAGMA user_version` is a 32-bit integer baked into the database file header for exactly
    /// this purpose (default `0`, no table needed, survives across opens) — reading it tells us
    /// which situation we're in:
    ///
    /// - **`0`, fresh database**: `init_schema` (called first, below) just created every table
    ///   at today's shape, so there is nothing to migrate — stamp `CURRENT_SCHEMA_VERSION` and
    ///   done.
    /// - **`0`, legacy database**: inspect its columns and apply the same entry-kind migration as
    ///   version 1 when needed.
    /// - **`== CURRENT_SCHEMA_VERSION`**: already up to date. Nothing to do.
    /// - **`> CURRENT_SCHEMA_VERSION`**: this database was written by a *newer* shoal, whose
    ///   schema shape this build doesn't understand. Refuse to open rather than risk silently
    ///   misreading or corrupting it — the caller needs to upgrade instead.
    ///
    pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        Self::init_schema(conn)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > CURRENT_SCHEMA_VERSION {
            return Err(io_to_sql(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "journal.db schema version {version} is newer than this shoal-journal build \
                     understands (max {CURRENT_SCHEMA_VERSION}); refusing to open — upgrade \
                     shoal before touching this database"
                ),
            )));
        }

        let has_kind = entry_has_column(conn, "kind")?;
        let has_parent = entry_has_column(conn, "parent_id")?;
        match (has_kind, has_parent) {
            (false, false) => migrate_entry_kind_columns(conn)?,
            (true, true) => {}
            _ => {
                return Err(io_to_sql(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "journal entry schema is partially migrated (kind/parent_id mismatch)",
                )));
            }
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_entry_parent ON entry(parent_id);
             CREATE INDEX IF NOT EXISTS idx_entry_owner_kind
                 ON entry(principal, session, kind, id);",
        )?;
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
        Ok(())
    }

    /// Create tables and indexes (idempotent). Schema per site/content/internals/language-conformance-contract.md.
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
                 opaque    BOOL    NOT NULL,
                 kind      TEXT    NOT NULL DEFAULT 'statement'
                           CHECK(kind IN ('statement', 'exec', 'approval')),
                 parent_id INTEGER
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
             CREATE TABLE IF NOT EXISTS transcript_event(
                 entry_id INTEGER PRIMARY KEY,
                 ts       INTEGER NOT NULL,
                 payload  TEXT    NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_entry_ts     ON entry(ts);
             CREATE INDEX IF NOT EXISTS idx_output_entry ON output(entry_id);
             CREATE INDEX IF NOT EXISTS idx_undo_entry   ON undo(entry_id);",
        )
    }

    /// Record an entry at execution start. Status, success verdict, and duration
    /// are `NULL` until [`Journal::finish`]. Returns the new entry id.
    pub fn append(&self, e: &EntryRecord) -> rusqlite::Result<i64> {
        let requested = entry_payload_bytes(e).saturating_add(DB_WRITE_RESERVE_BYTES);
        self.with_database_admission(requested, |tx| {
            tx.execute(
                "INSERT INTO entry (session, principal, ts, dur_ns, cwd, env_hash, src, ast, effects,
                                    status, ok, opaque, kind, parent_id)
                 VALUES (?1, ?2, ?3, NULL, ?4, NULL, ?5, ?6, ?7, NULL, NULL, ?8, ?9, ?10)",
                params![
                    e.session,
                    e.principal,
                    e.ts_ns,
                    e.cwd,
                    e.src,
                    e.ast_json,
                    e.effects_json,
                    e.opaque,
                    e.kind,
                    e.parent_id
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })
    }

    /// Record an entry whose outcome is already known in one atomic statement.
    ///
    /// Audit events do not have a running phase. Writing their completion
    /// columns together with the row avoids the `append`/`finish` gap where a
    /// crash or a second write failure could leave a grant looking unfinished.
    pub fn append_completed(
        &self,
        e: &EntryRecord,
        status: Option<i32>,
        ok: bool,
        dur_ns: i64,
    ) -> rusqlite::Result<i64> {
        let requested = entry_payload_bytes(e).saturating_add(DB_WRITE_RESERVE_BYTES);
        self.with_database_admission(requested, |tx| {
            tx.execute(
                "INSERT INTO entry (session, principal, ts, dur_ns, cwd, env_hash, src, ast, effects,
                                    status, ok, opaque, kind, parent_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    e.session,
                    e.principal,
                    e.ts_ns,
                    dur_ns,
                    e.cwd,
                    e.src,
                    e.ast_json,
                    e.effects_json,
                    status,
                    ok,
                    e.opaque,
                    e.kind,
                    e.parent_id
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })
    }

    /// Fill in the completion columns of a previously appended entry.
    ///
    /// `status` is `None` for signal deaths (site/content/internals/language-conformance-contract.md: never 128+n encoded).
    /// Errors with `StatementChangedRows(0)` if `id` does not exist.
    pub fn finish(
        &self,
        id: i64,
        status: Option<i32>,
        ok: bool,
        dur_ns: i64,
    ) -> rusqlite::Result<()> {
        // `with_database_admission` always retains the dedicated completion
        // reserve. Spend that reserve here instead of charging a second generic
        // write allowance: completion must remain possible after begin has
        // admitted the entry near the database ceiling.
        self.with_database_admission(0, |tx| {
            let changed = tx.execute(
                "UPDATE entry SET status = ?1, ok = ?2, dur_ns = ?3 WHERE id = ?4",
                params![status, ok, dur_ns, id],
            )?;
            if changed == 0 {
                return Err(rusqlite::Error::StatementChangedRows(0));
            }
            Ok(())
        })
    }
}

fn entry_has_column(conn: &Connection, wanted: &str) -> rusqlite::Result<bool> {
    let mut statement = conn.prepare("PRAGMA table_info(entry)")?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for column in columns {
        if column? == wanted {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Add semantic entry metadata to a pre-v2 database. Legacy parentage cannot
/// be reconstructed reliably, so it remains `NULL`; kind can be classified
/// from the exact shapes produced by older Shoal builds.
fn migrate_entry_kind_columns(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         ALTER TABLE entry ADD COLUMN kind TEXT NOT NULL DEFAULT 'statement'
             CHECK(kind IN ('statement', 'exec', 'approval'));
         ALTER TABLE entry ADD COLUMN parent_id INTEGER;
         UPDATE entry SET kind = CASE
             WHEN json_type(CASE WHEN json_valid(ast) THEN ast END, '$.stmts') = 'array'
                  THEN 'exec'
             WHEN ast = 'null'
                  AND json_extract(
                      CASE WHEN json_valid(effects) THEN effects ELSE '[]' END,
                      '$[0].kind'
                  ) = 'approval'
                  THEN 'approval'
             ELSE 'statement'
         END;
         COMMIT;",
    )
}

fn entry_payload_bytes(entry: &EntryRecord) -> u64 {
    [
        entry.session.len(),
        entry.principal.len(),
        entry.cwd.len(),
        entry.src.len(),
        entry.ast_json.len(),
        entry.effects_json.len(),
    ]
    .into_iter()
    .fold(0u64, |total, bytes| total.saturating_add(bytes as u64))
}
