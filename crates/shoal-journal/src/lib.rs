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

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, ToSql, params};
use serde::{Deserialize, Serialize};

/// Default number of rows returned by [`Journal::query`] when
/// [`JournalQuery::limit`] is `0`.
const DEFAULT_QUERY_LIMIT: usize = 100;

/// zstd compression level for CAS blobs (3 = the zstd default: fast, good ratio).
const ZSTD_LEVEL: i32 = 3;

/// Handle to a journal: a SQLite database plus an on-disk CAS directory.
///
/// Obtain one with [`Journal::open`] (persistent) or [`Journal::in_memory`]
/// (throwaway: in-memory database, temp-dir CAS that is deleted on drop).
pub struct Journal {
    conn: Connection,
    cas_root: PathBuf,
    /// Keeps the CAS temp dir alive for the lifetime of an in-memory journal.
    _cas_tempdir: Option<tempfile::TempDir>,
}

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

/// One captured output linked to an entry: a CAS blob reference.
#[derive(Debug, Clone)]
pub struct OutputRow {
    /// Output kind: `"stdout"`, `"stderr"`, `"value"`, or `"render"`.
    pub kind: String,
    /// blake3 hash of the (uncompressed) bytes, lowercase hex.
    pub hash: String,
    /// Length of the uncompressed bytes.
    pub len: i64,
}

/// A fully materialized journal entry as returned by [`Journal::query`].
///
/// `dur_ns`, `status`, and `ok` are `None` for entries that were appended but
/// never finished (still running, or interrupted by a crash).
#[derive(Debug, Clone)]
pub struct EntryRow {
    /// Rowid of the entry (stable reference, e.g. `out:12`).
    pub id: i64,
    /// Session identifier.
    pub session: String,
    /// Acting principal.
    pub principal: String,
    /// Start timestamp, nanoseconds since the Unix epoch.
    pub ts_ns: i64,
    /// Execution duration in nanoseconds, if finished.
    pub dur_ns: Option<i64>,
    /// Bytes of the working directory path.
    pub cwd: Vec<u8>,
    /// Source text as typed.
    pub src: String,
    /// Canonical AST JSON.
    pub ast_json: String,
    /// JSON array of effect instances.
    pub effects_json: String,
    /// Exit status, if finished with a normal exit.
    pub status: Option<i32>,
    /// Success verdict (per adapter `ok_codes`), if finished.
    pub ok: Option<bool>,
    /// Whether the effects were opaque.
    pub opaque: bool,
    /// Outputs linked to this entry, in recording order.
    pub outputs: Vec<OutputRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub size: u64,
    pub modified_ns: Option<u64>,
    pub hash: Option<String>,
}

impl FileFingerprint {
    pub fn capture(path: &Path) -> io::Result<Self> {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to fingerprint symlink",
            ));
        }
        let hash = if meta.is_file() {
            Some(blake3::hash(&fs::read(path)?).to_hex().to_string())
        } else {
            None
        };
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().min(u64::MAX as u128) as u64);
        Ok(Self {
            size: meta.len(),
            modified_ns,
            hash,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndoInverse {
    TrashMove {
        original: PathBuf,
        trash: PathBuf,
        trash_fingerprint: FileFingerprint,
    },
    RestoreBytes {
        path: PathBuf,
        prior_hash: String,
        expected_current: FileFingerprint,
    },
    MoveBack {
        from: PathBuf,
        to: PathBuf,
        expected_from: FileFingerprint,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoStatus {
    Applied,
    AlreadyApplied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoStep {
    pub inverse: UndoInverse,
    pub status: UndoStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoReport {
    pub entry_id: i64,
    pub steps: Vec<UndoStep>,
}

#[derive(Debug)]
pub enum UndoError {
    Sql(rusqlite::Error),
    Io(io::Error),
    Invalid(String),
    Escaped(PathBuf),
    Stale(PathBuf),
}
impl From<rusqlite::Error> for UndoError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}
impl From<io::Error> for UndoError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl std::fmt::Display for UndoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Invalid(e) => write!(f, "invalid undo inverse: {e}"),
            Self::Escaped(p) => write!(f, "undo target escapes scope: {}", p.display()),
            Self::Stale(p) => write!(
                f,
                "undo target was modified since recording: {}",
                p.display()
            ),
        }
    }
}
impl std::error::Error for UndoError {}

/// Filter set for [`Journal::query`]. `Default` matches everything with the
/// default limit.
#[derive(Default)]
pub struct JournalQuery {
    /// Only entries with `ts_ns >= since_ts_ns`.
    pub since_ts_ns: Option<i64>,
    /// Only entries whose `src`'s first whitespace-separated word equals this.
    pub head: Option<String>,
    /// Only entries recorded by this principal.
    pub principal: Option<String>,
    /// Only finished entries with this success verdict (unfinished entries have
    /// `NULL` ok and never match).
    pub ok: Option<bool>,
    /// Maximum rows returned; `0` means the default of 100.
    pub limit: usize,
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

impl Journal {
    /// Open (creating if needed) the journal under `state_dir`.
    ///
    /// Creates the directory tree, `<state_dir>/journal.db` in WAL mode, and
    /// `<state_dir>/cas/`.
    pub fn open(state_dir: &Path) -> rusqlite::Result<Journal> {
        let cas_root = state_dir.join("cas");
        fs::create_dir_all(&cas_root).map_err(io_to_sql)?;
        let conn = Connection::open(state_dir.join("journal.db"))?;
        // `PRAGMA journal_mode=WAL` returns a result row; consume it.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init_schema(&conn)?;
        Ok(Journal {
            conn,
            cas_root,
            _cas_tempdir: None,
        })
    }

    /// Open a throwaway journal: in-memory SQLite database, CAS in a fresh
    /// temporary directory that lives exactly as long as the returned `Journal`.
    pub fn in_memory() -> rusqlite::Result<Journal> {
        let tempdir = tempfile::tempdir().map_err(io_to_sql)?;
        let cas_root = tempdir.path().join("cas");
        fs::create_dir_all(&cas_root).map_err(io_to_sql)?;
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Journal {
            conn,
            cas_root,
            _cas_tempdir: Some(tempdir),
        })
    }

    /// Create tables and indexes (idempotent). Schema per TDD §9.
    fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
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

    /// Store `bytes` in the CAS (zstd-compressed, blake3-addressed, deduplicated)
    /// and link them to entry `id`. `kind` is `"stdout"`, `"stderr"`, `"value"`,
    /// or `"render"`. Returns the blake3 hash as lowercase hex.
    ///
    /// Identical bytes recorded twice produce two `output` rows but a single CAS
    /// file. The blob is written atomically (temp file + rename) before the row
    /// insert; a crash in between leaves at worst an unreferenced blob for GC.
    pub fn record_output(&self, id: i64, kind: &str, bytes: &[u8]) -> rusqlite::Result<String> {
        let hash = blake3::hash(bytes);
        let hex = hash.to_hex().to_string();
        let path = self.blob_path(&hex);
        if !path.exists() {
            let parent = path.parent().expect("blob path always has a parent");
            fs::create_dir_all(parent).map_err(io_to_sql)?;
            let compressed = zstd::encode_all(bytes, ZSTD_LEVEL).map_err(io_to_sql)?;
            let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(io_to_sql)?;
            tmp.write_all(&compressed).map_err(io_to_sql)?;
            tmp.persist(&path).map_err(|e| io_to_sql(e.error))?;
        }
        self.conn.execute(
            "INSERT INTO output (entry_id, kind, hash, len, meta) VALUES (?1, ?2, ?3, ?4, NULL)",
            params![id, kind, hash.as_bytes().as_slice(), bytes.len() as i64],
        )?;
        Ok(hex)
    }

    /// Fetch and decompress a CAS blob by its blake3 hex hash.
    ///
    /// Returns `Ok(None)` when the blob does not exist (including malformed hash
    /// strings, which cannot name a blob).
    pub fn read_blob(&self, hash: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        // A hash that is not plain hex (or is too short to shard) cannot address
        // a blob; this also guards against path traversal.
        if hash.len() < 4 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let compressed = match fs::read(self.blob_path(hash)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_to_sql(e)),
        };
        let bytes = zstd::decode_all(compressed.as_slice()).map_err(io_to_sql)?;
        Ok(Some(bytes))
    }

    /// Query entries newest-first with the filters in `q`, outputs joined in.
    ///
    /// `limit == 0` means the default of 100. The `head` filter matches entries
    /// whose `src`'s first whitespace-separated word equals `head` exactly.
    pub fn query(&self, q: &JournalQuery) -> rusqlite::Result<Vec<EntryRow>> {
        let limit = if q.limit == 0 {
            DEFAULT_QUERY_LIMIT
        } else {
            q.limit
        };
        let limit_i64 = limit as i64;

        let mut sql = String::from(
            "SELECT id, session, principal, ts, dur_ns, cwd, src, ast, effects, status, ok, opaque
             FROM entry",
        );
        let mut clauses: Vec<&str> = Vec::new();
        let mut params: Vec<&dyn ToSql> = Vec::new();
        if let Some(ts) = q.since_ts_ns.as_ref() {
            clauses.push("ts >= ?");
            params.push(ts);
        }
        if let Some(principal) = q.principal.as_ref() {
            clauses.push("principal = ?");
            params.push(principal);
        }
        if let Some(ok) = q.ok.as_ref() {
            clauses.push("ok = ?");
            params.push(ok);
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY id DESC");
        // The head filter is applied in Rust (SQL cannot cheaply split on arbitrary
        // whitespace), so SQL LIMIT is only usable when no head filter is set.
        if q.head.is_none() {
            sql.push_str(" LIMIT ?");
            params.push(&limit_i64);
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params.as_slice())?;
        let mut out: Vec<EntryRow> = Vec::new();
        while let Some(row) = rows.next()? {
            let src: String = row.get(6)?;
            if let Some(head) = &q.head
                && src.split_whitespace().next() != Some(head.as_str())
            {
                continue;
            }
            out.push(EntryRow {
                id: row.get(0)?,
                session: row.get(1)?,
                principal: row.get(2)?,
                ts_ns: row.get(3)?,
                dur_ns: row.get(4)?,
                cwd: row.get(5)?,
                src,
                ast_json: row.get(7)?,
                effects_json: row.get(8)?,
                status: row.get(9)?,
                ok: row.get(10)?,
                opaque: row.get(11)?,
                outputs: Vec::new(),
            });
            if out.len() >= limit {
                break;
            }
        }

        let mut out_stmt = self
            .conn
            .prepare("SELECT kind, hash, len FROM output WHERE entry_id = ?1 ORDER BY rowid")?;
        for entry in &mut out {
            entry.outputs = out_stmt
                .query_map([entry.id], |r| {
                    let raw: Vec<u8> = r.get(1)?;
                    Ok(OutputRow {
                        kind: r.get(0)?,
                        hash: hex_string(&raw),
                        len: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(out)
    }

    /// Record an undo inverse for entry `id`. `op` names the inverse operation
    /// (`"trash"`, `"restore_bytes"`, …); `inverse_json` is its JSON payload.
    pub fn record_undo(&self, id: i64, op: &str, inverse_json: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO undo (entry_id, op, inverse) VALUES (?1, ?2, ?3)",
            params![id, op, inverse_json],
        )?;
        Ok(())
    }

    pub fn record_undo_inverse(&self, id: i64, inverse: &UndoInverse) -> rusqlite::Result<()> {
        let json = serde_json::to_string(inverse)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.record_undo(id, inverse_name(inverse), &json)
    }

    /// Replay typed inverses newest-first. Destinations must remain inside
    /// `root`; stale fingerprints and symlink traversal are hard failures.
    pub fn undo_entry(&self, id: i64, root: &Path) -> Result<UndoReport, UndoError> {
        let root = root.canonicalize()?;
        let mut stmt = self
            .conn
            .prepare("SELECT inverse FROM undo WHERE entry_id=?1 ORDER BY rowid DESC")?;
        let encoded = stmt
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut steps = Vec::new();
        for json in encoded {
            let inverse: UndoInverse =
                serde_json::from_str(&json).map_err(|e| UndoError::Invalid(e.to_string()))?;
            let status = self.apply_inverse(&inverse, &root)?;
            steps.push(UndoStep { inverse, status });
        }
        Ok(UndoReport {
            entry_id: id,
            steps,
        })
    }

    fn apply_inverse(&self, inverse: &UndoInverse, root: &Path) -> Result<UndoStatus, UndoError> {
        match inverse {
            UndoInverse::TrashMove {
                original,
                trash,
                trash_fingerprint,
            } => {
                checked_target(root, original)?;
                if !trash.exists() {
                    return if original.exists() {
                        Ok(UndoStatus::AlreadyApplied)
                    } else {
                        Err(UndoError::Stale(trash.clone()))
                    };
                }
                require_fingerprint(trash, trash_fingerprint)?;
                if original.exists() {
                    return Err(UndoError::Stale(original.clone()));
                }
                ensure_no_symlink_parents(root, original)?;
                fs::rename(trash, original)?;
                Ok(UndoStatus::Applied)
            }
            UndoInverse::RestoreBytes {
                path,
                prior_hash,
                expected_current,
            } => {
                checked_target(root, path)?;
                let prior = self
                    .read_blob(prior_hash)?
                    .ok_or_else(|| UndoError::Invalid(format!("missing CAS blob {prior_hash}")))?;
                if path.exists() {
                    let current = FileFingerprint::capture(path)?;
                    if current.hash.as_deref() == Some(blake3::hash(&prior).to_hex().as_str()) {
                        return Ok(UndoStatus::AlreadyApplied);
                    }
                    if &current != expected_current {
                        return Err(UndoError::Stale(path.clone()));
                    }
                } else {
                    return Err(UndoError::Stale(path.clone()));
                }
                ensure_no_symlink_parents(root, path)?;
                atomic_replace(path, &prior)?;
                Ok(UndoStatus::Applied)
            }
            UndoInverse::MoveBack {
                from,
                to,
                expected_from,
            } => {
                checked_target(root, from)?;
                checked_target(root, to)?;
                if !from.exists() {
                    return if to.exists() {
                        Ok(UndoStatus::AlreadyApplied)
                    } else {
                        Err(UndoError::Stale(from.clone()))
                    };
                }
                require_fingerprint(from, expected_from)?;
                if to.exists() {
                    return Err(UndoError::Stale(to.clone()));
                }
                ensure_no_symlink_parents(root, to)?;
                fs::rename(from, to)?;
                Ok(UndoStatus::Applied)
            }
        }
    }

    /// List `(op, inverse_json)` undo records for entry `id`, in recording order.
    /// (`undo out[n]` replays these newest-first — callers reverse.)
    pub fn undos_for(&self, id: i64) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT op, inverse FROM undo WHERE entry_id = ?1 ORDER BY rowid")?;
        let rows = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Absolute path of the CAS blob for a hex hash:
    /// `<cas>/<hex[0..2]>/<hex[2..4]>/<hex>.zst`.
    fn blob_path(&self, hex: &str) -> PathBuf {
        self.cas_root
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(format!("{hex}.zst"))
    }
}

fn inverse_name(inverse: &UndoInverse) -> &'static str {
    match inverse {
        UndoInverse::TrashMove { .. } => "trash_move",
        UndoInverse::RestoreBytes { .. } => "restore_bytes",
        UndoInverse::MoveBack { .. } => "move_back",
    }
}

fn checked_target(root: &Path, path: &Path) -> Result<(), UndoError> {
    if !path.is_absolute() || !path.starts_with(root) {
        return Err(UndoError::Escaped(path.to_owned()));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            c => normalized.push(c.as_os_str()),
        }
    }
    if !normalized.starts_with(root) {
        return Err(UndoError::Escaped(path.to_owned()));
    }
    Ok(())
}

fn ensure_no_symlink_parents(root: &Path, path: &Path) -> Result<(), UndoError> {
    let parent = path
        .parent()
        .ok_or_else(|| UndoError::Escaped(path.to_owned()))?;
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| UndoError::Escaped(path.to_owned()))?;
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(UndoError::Escaped(current)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn require_fingerprint(path: &Path, expected: &FileFingerprint) -> Result<(), UndoError> {
    if &FileFingerprint::capture(path)? == expected {
        Ok(())
    } else {
        Err(UndoError::Stale(path.to_owned()))
    }
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(session: &str, principal: &str, ts_ns: i64, src: &str) -> EntryRecord {
        EntryRecord {
            session: session.to_string(),
            principal: principal.to_string(),
            ts_ns,
            cwd: b"/home/user/proj".to_vec(),
            src: src.to_string(),
            ast_json: r#"{"kind":"call","cmd":"x"}"#.to_string(),
            effects_json: r#"["opaque"]"#.to_string(),
            opaque: true,
        }
    }

    /// Count regular files under `dir`, recursively.
    fn count_files(dir: &Path) -> usize {
        let mut n = 0;
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                n += count_files(&entry.path());
            } else {
                n += 1;
            }
        }
        n
    }

    #[test]
    fn append_finish_query_roundtrip() {
        let j = Journal::in_memory().unwrap();
        let e = rec("s1", "human", 1_000, "git push origin main");
        let id = j.append(&e).unwrap();
        assert_eq!(id, 1);

        // Before finish: NULL status/ok/dur.
        let rows = j.query(&JournalQuery::default()).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.id, id);
        assert_eq!(r.session, "s1");
        assert_eq!(r.principal, "human");
        assert_eq!(r.ts_ns, 1_000);
        assert_eq!(r.cwd, b"/home/user/proj".to_vec());
        assert_eq!(r.src, "git push origin main");
        assert_eq!(r.ast_json, r#"{"kind":"call","cmd":"x"}"#);
        assert_eq!(r.effects_json, r#"["opaque"]"#);
        assert!(r.opaque);
        assert_eq!(r.status, None);
        assert_eq!(r.ok, None);
        assert_eq!(r.dur_ns, None);
        assert!(r.outputs.is_empty());

        j.finish(id, Some(0), true, 42_000_000).unwrap();
        let rows = j.query(&JournalQuery::default()).unwrap();
        let r = &rows[0];
        assert_eq!(r.status, Some(0));
        assert_eq!(r.ok, Some(true));
        assert_eq!(r.dur_ns, Some(42_000_000));
    }

    #[test]
    fn finish_unknown_id_errors() {
        let j = Journal::in_memory().unwrap();
        let err = j.finish(999, Some(0), true, 1).unwrap_err();
        assert!(matches!(err, rusqlite::Error::StatementChangedRows(0)));
    }

    #[test]
    fn unfinished_entry_survives_reopen_with_null_status() {
        // WAL crash-tolerance smoke: append, drop without finish, reopen.
        let dir = tempfile::tempdir().unwrap();
        let id;
        {
            let j = Journal::open(dir.path()).unwrap();
            id = j.append(&rec("s1", "human", 5, "sleep 100")).unwrap();
            // Dropped without finish — simulates a crash mid-execution.
        }
        let j = Journal::open(dir.path()).unwrap();
        let rows = j.query(&JournalQuery::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].src, "sleep 100");
        assert_eq!(rows[0].status, None);
        assert_eq!(rows[0].ok, None);
        assert_eq!(rows[0].dur_ns, None);
    }

    #[test]
    fn open_creates_tree_and_wal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("deep").join("state");
        {
            let j = Journal::open(&state).unwrap();
            j.append(&rec("s", "human", 1, "ls")).unwrap();
        }
        assert!(state.join("journal.db").is_file());
        assert!(state.join("cas").is_dir());
        // WAL mode is persisted in the database header.
        let conn = Connection::open(state.join("journal.db")).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn cas_roundtrip_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path()).unwrap();
        let id = j.append(&rec("s", "human", 1, "cat big.log")).unwrap();

        let payload = b"hello CAS world\nline two\n".repeat(100);
        let h1 = j.record_output(id, "stdout", &payload).unwrap();
        let h2 = j.record_output(id, "stderr", &payload).unwrap();
        assert_eq!(h1, h2, "identical bytes must hash identically");
        assert_eq!(h1, blake3::hash(&payload).to_hex().to_string());

        // Same bytes twice -> exactly one file in the CAS.
        assert_eq!(count_files(&dir.path().join("cas")), 1);
        // Sharded layout: cas/<hex[0..2]>/<hex[2..4]>/<hex>.zst
        let blob = dir
            .path()
            .join("cas")
            .join(&h1[0..2])
            .join(&h1[2..4])
            .join(format!("{h1}.zst"));
        assert!(blob.is_file());
        // Stored compressed, not raw.
        let on_disk = fs::read(&blob).unwrap();
        assert_ne!(on_disk, payload);
        assert!(on_disk.len() < payload.len());

        // Roundtrip through read_blob.
        assert_eq!(j.read_blob(&h1).unwrap().unwrap(), payload);

        // Both output rows are linked and joined by query.
        let rows = j.query(&JournalQuery::default()).unwrap();
        let outs = &rows[0].outputs;
        assert_eq!(outs.len(), 2);
        assert_eq!(outs[0].kind, "stdout");
        assert_eq!(outs[1].kind, "stderr");
        assert!(
            outs.iter()
                .all(|o| o.hash == h1 && o.len == payload.len() as i64)
        );
    }

    #[test]
    fn distinct_bytes_get_distinct_files() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path()).unwrap();
        let id = j.append(&rec("s", "human", 1, "x")).unwrap();
        let h1 = j.record_output(id, "stdout", b"alpha").unwrap();
        let h2 = j.record_output(id, "stdout", b"beta").unwrap();
        assert_ne!(h1, h2);
        assert_eq!(count_files(&dir.path().join("cas")), 2);
        assert_eq!(j.read_blob(&h1).unwrap().unwrap(), b"alpha");
        assert_eq!(j.read_blob(&h2).unwrap().unwrap(), b"beta");
    }

    #[test]
    fn record_output_empty_bytes() {
        let j = Journal::in_memory().unwrap();
        let id = j.append(&rec("s", "human", 1, "true")).unwrap();
        let h = j.record_output(id, "stdout", b"").unwrap();
        assert_eq!(j.read_blob(&h).unwrap().unwrap(), Vec::<u8>::new());
        let rows = j.query(&JournalQuery::default()).unwrap();
        assert_eq!(rows[0].outputs[0].len, 0);
    }

    #[test]
    fn read_blob_missing_returns_none() {
        let j = Journal::in_memory().unwrap();
        // Well-formed hash that was never stored.
        let absent = blake3::hash(b"never stored").to_hex().to_string();
        assert_eq!(j.read_blob(&absent).unwrap(), None);
        // Malformed hashes cannot name blobs.
        assert_eq!(j.read_blob("").unwrap(), None);
        assert_eq!(j.read_blob("zz").unwrap(), None);
        assert_eq!(j.read_blob("../../etc/passwd").unwrap(), None);
    }

    #[test]
    fn query_head_filter() {
        let j = Journal::in_memory().unwrap();
        j.append(&rec("s", "human", 1, "git push origin main"))
            .unwrap();
        j.append(&rec("s", "human", 2, "cargo build --release"))
            .unwrap();
        j.append(&rec("s", "human", 3, "gitk --all")).unwrap();
        j.append(&rec("s", "human", 4, "  git   status")).unwrap(); // leading whitespace ok

        let q = JournalQuery {
            head: Some("git".to_string()),
            ..JournalQuery::default()
        };
        let rows = j.query(&q).unwrap();
        assert_eq!(rows.len(), 2, "prefix match ('gitk') must not count");
        assert_eq!(rows[0].src, "  git   status"); // newest first
        assert_eq!(rows[1].src, "git push origin main");
    }

    #[test]
    fn query_principal_filter() {
        let j = Journal::in_memory().unwrap();
        j.append(&rec("s", "human", 1, "ls")).unwrap();
        j.append(&rec("s", "agent:refactor", 2, "cargo test"))
            .unwrap();
        j.append(&rec("s", "human", 3, "pwd")).unwrap();

        let q = JournalQuery {
            principal: Some("agent:refactor".to_string()),
            ..JournalQuery::default()
        };
        let rows = j.query(&q).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].src, "cargo test");
    }

    #[test]
    fn query_ok_filter_excludes_unfinished() {
        let j = Journal::in_memory().unwrap();
        let ok_id = j.append(&rec("s", "human", 1, "true")).unwrap();
        let bad_id = j.append(&rec("s", "human", 2, "false")).unwrap();
        j.append(&rec("s", "human", 3, "sleep 999")).unwrap(); // never finished
        j.finish(ok_id, Some(0), true, 10).unwrap();
        j.finish(bad_id, Some(1), false, 20).unwrap();

        let q_ok = JournalQuery {
            ok: Some(true),
            ..JournalQuery::default()
        };
        let rows = j.query(&q_ok).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, ok_id);

        let q_bad = JournalQuery {
            ok: Some(false),
            ..JournalQuery::default()
        };
        let rows = j.query(&q_bad).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, bad_id);

        // No ok filter: unfinished entry included.
        assert_eq!(j.query(&JournalQuery::default()).unwrap().len(), 3);
    }

    #[test]
    fn query_since_ts_filter() {
        let j = Journal::in_memory().unwrap();
        j.append(&rec("s", "human", 100, "old")).unwrap();
        j.append(&rec("s", "human", 200, "mid")).unwrap();
        j.append(&rec("s", "human", 300, "new")).unwrap();

        let q = JournalQuery {
            since_ts_ns: Some(200),
            ..JournalQuery::default()
        };
        let rows = j.query(&q).unwrap();
        assert_eq!(rows.len(), 2, "since is inclusive");
        assert_eq!(rows[0].src, "new");
        assert_eq!(rows[1].src, "mid");
    }

    #[test]
    fn query_limit_and_order() {
        let j = Journal::in_memory().unwrap();
        for i in 0..5 {
            j.append(&rec("s", "human", i, &format!("cmd{i}"))).unwrap();
        }
        let q = JournalQuery {
            limit: 2,
            ..JournalQuery::default()
        };
        let rows = j.query(&q).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].src, "cmd4"); // ORDER BY id DESC
        assert_eq!(rows[1].src, "cmd3");
        assert!(rows[0].id > rows[1].id);
    }

    #[test]
    fn query_default_limit_is_100() {
        let j = Journal::in_memory().unwrap();
        for i in 0..105 {
            j.append(&rec("s", "human", i, "echo hi")).unwrap();
        }
        let rows = j.query(&JournalQuery::default()).unwrap();
        assert_eq!(rows.len(), 100);
        assert_eq!(rows[0].ts_ns, 104); // newest first
    }

    #[test]
    fn head_filter_respects_limit() {
        let j = Journal::in_memory().unwrap();
        for i in 0..4 {
            j.append(&rec("s", "human", i * 2, &format!("git commit -m {i}")))
                .unwrap();
            j.append(&rec("s", "human", i * 2 + 1, "ls -la")).unwrap();
        }
        let q = JournalQuery {
            head: Some("git".to_string()),
            limit: 2,
            ..JournalQuery::default()
        };
        let rows = j.query(&q).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].src, "git commit -m 3");
        assert_eq!(rows[1].src, "git commit -m 2");
    }

    #[test]
    fn combined_filters() {
        let j = Journal::in_memory().unwrap();
        let a = j.append(&rec("s", "agent:x", 10, "git push")).unwrap();
        let b = j.append(&rec("s", "human", 20, "git push")).unwrap();
        let c = j.append(&rec("s", "agent:x", 30, "git pull")).unwrap();
        for id in [a, b, c] {
            j.finish(id, Some(0), true, 1).unwrap();
        }
        let q = JournalQuery {
            head: Some("git".to_string()),
            principal: Some("agent:x".to_string()),
            ok: Some(true),
            since_ts_ns: Some(15),
            ..JournalQuery::default()
        };
        let rows = j.query(&q).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, c);
    }

    #[test]
    fn undo_record_and_list() {
        let j = Journal::in_memory().unwrap();
        let id = j.append(&rec("s", "human", 1, "rm -rf build")).unwrap();
        let other = j.append(&rec("s", "human", 2, "ls")).unwrap();

        let inv1 = serde_json::json!({"trash": "/home/user/.trash/build"}).to_string();
        let inv2 =
            serde_json::json!({"restore_bytes": {"path": "a.txt", "hash": "ab"}}).to_string();
        j.record_undo(id, "trash", &inv1).unwrap();
        j.record_undo(id, "restore_bytes", &inv2).unwrap();

        let undos = j.undos_for(id).unwrap();
        assert_eq!(undos.len(), 2);
        assert_eq!(undos[0], ("trash".to_string(), inv1.clone()));
        assert_eq!(undos[1], ("restore_bytes".to_string(), inv2.clone()));
        // Payload survives as valid JSON.
        let parsed: serde_json::Value = serde_json::from_str(&undos[0].1).unwrap();
        assert_eq!(parsed["trash"], "/home/user/.trash/build");

        assert!(j.undos_for(other).unwrap().is_empty());
        assert!(j.undos_for(9999).unwrap().is_empty());
    }

    #[test]
    fn in_memory_cas_lives_with_journal() {
        let j = Journal::in_memory().unwrap();
        let id = j.append(&rec("s", "human", 1, "echo hi")).unwrap();
        let h = j.record_output(id, "stdout", b"hi\n").unwrap();
        // The tempdir CAS must still be readable as long as the Journal lives.
        assert_eq!(j.read_blob(&h).unwrap().unwrap(), b"hi\n");
    }

    #[test]
    fn undo_trash_move_restores_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let trash_dir = tempfile::tempdir().unwrap();
        let original = root.path().join("gone.txt");
        let trash = trash_dir.path().join("gone.txt");
        fs::write(&original, b"important").unwrap();
        fs::rename(&original, &trash).unwrap();
        let inverse = UndoInverse::TrashMove {
            original: original.clone(),
            trash: trash.clone(),
            trash_fingerprint: FileFingerprint::capture(&trash).unwrap(),
        };
        let j = Journal::in_memory().unwrap();
        let id = j.append(&rec("s", "human", 1, "rm gone.txt")).unwrap();
        j.record_undo_inverse(id, &inverse).unwrap();
        let report = j.undo_entry(id, root.path()).unwrap();
        assert_eq!(report.steps[0].status, UndoStatus::Applied);
        assert_eq!(fs::read(&original).unwrap(), b"important");
        assert_eq!(
            j.undo_entry(id, root.path()).unwrap().steps[0].status,
            UndoStatus::AlreadyApplied
        );
    }

    #[test]
    fn undo_restore_bytes_refuses_stale_content() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config");
        fs::write(&path, b"before").unwrap();
        let j = Journal::in_memory().unwrap();
        let id = j.append(&rec("s", "human", 1, "save config")).unwrap();
        let prior = j.record_output(id, "value", b"before").unwrap();
        fs::write(&path, b"after").unwrap();
        let inverse = UndoInverse::RestoreBytes {
            path: path.clone(),
            prior_hash: prior,
            expected_current: FileFingerprint::capture(&path).unwrap(),
        };
        j.record_undo_inverse(id, &inverse).unwrap();
        fs::write(&path, b"user edit").unwrap();
        assert!(matches!(j.undo_entry(id,root.path()),Err(UndoError::Stale(p)) if p==path));
        assert_eq!(fs::read(&path).unwrap(), b"user edit");
    }

    #[test]
    fn undo_restore_bytes_uses_cas() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config");
        fs::write(&path, b"before").unwrap();
        let j = Journal::in_memory().unwrap();
        let id = j.append(&rec("s", "human", 1, "save config")).unwrap();
        let prior = j.record_output(id, "value", b"before").unwrap();
        fs::write(&path, b"after").unwrap();
        j.record_undo_inverse(
            id,
            &UndoInverse::RestoreBytes {
                path: path.clone(),
                prior_hash: prior,
                expected_current: FileFingerprint::capture(&path).unwrap(),
            },
        )
        .unwrap();
        assert_eq!(
            j.undo_entry(id, root.path()).unwrap().steps[0].status,
            UndoStatus::Applied
        );
        assert_eq!(fs::read(&path).unwrap(), b"before");
        assert_eq!(
            j.undo_entry(id, root.path()).unwrap().steps[0].status,
            UndoStatus::AlreadyApplied
        );
    }

    #[test]
    fn undo_replays_moves_newest_first() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        let c = root.path().join("c");
        fs::write(&a, b"x").unwrap();
        fs::rename(&a, &b).unwrap();
        let fp = FileFingerprint::capture(&b).unwrap();
        let j = Journal::in_memory().unwrap();
        let id = j.append(&rec("s", "human", 1, "mv a b; mv b c")).unwrap();
        j.record_undo_inverse(
            id,
            &UndoInverse::MoveBack {
                from: b.clone(),
                to: a.clone(),
                expected_from: fp.clone(),
            },
        )
        .unwrap();
        fs::rename(&b, &c).unwrap();
        j.record_undo_inverse(
            id,
            &UndoInverse::MoveBack {
                from: c,
                to: b,
                expected_from: fp,
            },
        )
        .unwrap();
        let report = j.undo_entry(id, root.path()).unwrap();
        assert_eq!(report.steps.len(), 2);
        assert!(a.exists());
    }

    #[cfg(unix)]
    #[test]
    fn undo_rejects_traversal_and_symlink_parent() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let j = Journal::in_memory().unwrap();
        let id = j.append(&rec("s", "human", 1, "undo hostile")).unwrap();
        let escaped = root.path().join("..").join("escape");
        j.record_undo_inverse(
            id,
            &UndoInverse::MoveBack {
                from: escaped.clone(),
                to: root.path().join("safe"),
                expected_from: FileFingerprint {
                    size: 0,
                    modified_ns: None,
                    hash: None,
                },
            },
        )
        .unwrap();
        assert!(matches!(
            j.undo_entry(id, root.path()),
            Err(UndoError::Escaped(_))
        ));
        let id2 = j.append(&rec("s", "human", 2, "undo symlink")).unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();
        let target = root.path().join("link/file");
        fs::write(outside.path().join("file"), b"after").unwrap();
        let prior = j.record_output(id2, "value", b"before").unwrap();
        j.record_undo_inverse(
            id2,
            &UndoInverse::RestoreBytes {
                path: target.clone(),
                prior_hash: prior,
                expected_current: FileFingerprint::capture(&target).unwrap(),
            },
        )
        .unwrap();
        assert!(matches!(
            j.undo_entry(id2, root.path()),
            Err(UndoError::Escaped(_))
        ));
        assert_eq!(fs::read(outside.path().join("file")).unwrap(), b"after");
    }
}
