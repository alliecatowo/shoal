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
//! Every table so far has been created with `CREATE TABLE IF NOT EXISTS`, and every change to
//! date (e.g. adding `transcript_event`) has been additive, so no version marker was ever kept.
//! [`Journal::migrate`] adds the scaffold a future *non-additive* change (renaming/dropping a
//! column, restructuring a table) will need: it stamps SQLite's built-in `PRAGMA user_version` to
//! [`CURRENT_SCHEMA_VERSION`] on every open, so an old-vs-new on-disk schema can be told apart
//! before it's misread. See [`Journal::migrate`]'s doc comment for exactly how to add the first
//! real migration.

use std::io;

use rusqlite::{Connection, params};

use crate::{Journal, io_to_sql};

/// The schema version this build of `shoal-journal` understands, stamped into
/// `PRAGMA user_version` by [`Journal::migrate`] on every open.
///
/// Bump this — and extend the `match` in [`Journal::migrate`] — only when a schema change is
/// genuinely non-additive. A purely additive change (a new `CREATE TABLE IF NOT EXISTS`, like
/// `transcript_event` was) needs no bump: [`Journal::init_schema`] already re-runs on every open
/// and is idempotent, so an old database just gains the new table in place.
pub(crate) const CURRENT_SCHEMA_VERSION: i64 = 1;

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
    /// which of four situations we're in:
    ///
    /// - **`0`, fresh database**: `init_schema` (called first, below) just created every table
    ///   at today's shape, so there is nothing to migrate — stamp `CURRENT_SCHEMA_VERSION` and
    ///   done.
    /// - **`0`, legacy database**: a `journal.db` written before this scaffold existed. Its tables
    ///   already exist and already match today's shape, because every change to date has been
    ///   additive (`CREATE TABLE IF NOT EXISTS`, e.g. `transcript_event`) and `init_schema` just
    ///   re-ran and is a no-op past table-creation. So this case is indistinguishable from — and
    ///   handled identically to — the fresh-database case: stamp the version. Zero data loss,
    ///   zero DDL, because there is genuinely nothing to change.
    /// - **`== CURRENT_SCHEMA_VERSION`**: already up to date. Nothing to do.
    /// - **`> CURRENT_SCHEMA_VERSION`**: this database was written by a *newer* shoal, whose
    ///   schema shape this build doesn't understand. Refuse to open rather than risk silently
    ///   misreading or corrupting it — the caller needs to upgrade instead.
    ///
    /// A fifth case, `1..CURRENT_SCHEMA_VERSION`, is where a real migration will one day live. It
    /// is unreachable today (`CURRENT_SCHEMA_VERSION` is still `1`, so no version in that range
    /// can exist), but is wired up and documented so the first migration doesn't have to touch
    /// this dispatch's shape. **To add migration `N -> N+1`:**
    ///
    /// 1. Bump [`CURRENT_SCHEMA_VERSION`] to `N + 1`.
    /// 2. Add a match arm below (in the `_` arm's place, or ahead of it if there are several
    ///    versions to step through) keyed on the version, e.g. `n if n == N => { conn
    ///    .execute_batch("ALTER TABLE ...")?; }` — one self-contained, tested transformation per
    ///    version, not a jump straight to `CURRENT_SCHEMA_VERSION`. Prefer a `while version <
    ///    CURRENT_SCHEMA_VERSION` loop over a `match` once there is more than one step to chain.
    /// 3. Leave the final `PRAGMA user_version` write (below) to stamp `CURRENT_SCHEMA_VERSION`
    ///    once every step has run — don't stamp inside the arm itself.
    /// 4. Add a `tests.rs` case that hand-builds a version-`N` fixture database (see
    ///    `legacy_zero_version_db_is_adopted_without_losing_rows` for the pattern of building a
    ///    fixture by hand and asserting both the new shape AND that pre-existing rows survive).
    pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        Self::init_schema(conn)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            // Fresh database, or a legacy pre-versioning one whose tables already match (see
            // the doc comment above) — either way there is nothing to migrate.
            0 => {}
            v if v == CURRENT_SCHEMA_VERSION => return Ok(()),
            v if v > CURRENT_SCHEMA_VERSION => {
                return Err(io_to_sql(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "journal.db schema version {v} is newer than this shoal-journal build \
                         understands (max {CURRENT_SCHEMA_VERSION}); refusing to open — upgrade \
                         shoal before touching this database"
                    ),
                )));
            }
            // 1..CURRENT_SCHEMA_VERSION: the future-migration dispatch point. No migration has
            // ever shipped (CURRENT_SCHEMA_VERSION is still 1), so this arm is currently
            // unreachable. See the doc comment above for exactly how to add one here.
            _ => {}
        }
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
        self.conn.execute(
            "INSERT INTO entry (session, principal, ts, dur_ns, cwd, env_hash, src, ast, effects,
                                status, ok, opaque)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                e.opaque
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
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
