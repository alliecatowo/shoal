//! Filtered, newest-first entry queries with joined outputs.

use rusqlite::ToSql;

use crate::{Journal, OutputMeta, OutputRow, hex_string};

/// Default number of rows returned by [`Journal::query`] when
/// [`JournalQuery::limit`] is `0`.
const DEFAULT_QUERY_LIMIT: usize = 100;

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

impl Journal {
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

        let mut out_stmt = self.conn.prepare(
            "SELECT kind, hash, len, meta FROM output WHERE entry_id = ?1 ORDER BY rowid",
        )?;
        for entry in &mut out {
            entry.outputs = out_stmt
                .query_map([entry.id], |r| {
                    let raw: Vec<u8> = r.get(1)?;
                    let meta_json: Option<String> = r.get(3)?;
                    let meta: Option<OutputMeta> = meta_json
                        .map(|json| serde_json::from_str(&json))
                        .transpose()
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?;
                    Ok(OutputRow {
                        kind: r.get(0)?,
                        hash: hex_string(&raw),
                        len: r.get(2)?,
                        meta,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(out)
    }
}
