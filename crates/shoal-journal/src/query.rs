//! Filtered, newest-first entry queries with joined outputs, plus a targeted
//! by-id fetch for callers that already know exactly which rows they want.

use std::collections::HashMap;

use rusqlite::ToSql;

use crate::{EntryKind, Journal, OutputMeta, OutputRow, hash_string, hex_bytes};

/// Default number of rows returned by [`Journal::query`] when
/// [`JournalQuery::limit`] is `0`.
const DEFAULT_QUERY_LIMIT: usize = 100;

fn sql_i64_from_usize(value: usize) -> rusqlite::Result<i64> {
    i64::try_from(value).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn sql_i64_from_u64(value: u64) -> rusqlite::Result<i64> {
    i64::try_from(value).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn count_as_u64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

/// A fully materialized journal entry as returned by [`Journal::query`].
///
/// `dur_ns`, `status`, and `ok` are `None` for entries that were appended but
/// never finished (still running, or interrupted by a crash).
#[derive(Debug, Clone)]
pub struct EntryRow {
    /// Rowid of the entry (stable reference, e.g. `out:12`).
    pub id: i64,
    /// Semantic role of this row.
    pub kind: EntryKind,
    /// Owning coarse execution row, for evaluator statements.
    pub parent_id: Option<i64>,
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

/// Bounded cursor state used to lazily hydrate one durable event channel.
/// `published` is the full historical count; `tail_entry_ids` contains only
/// the newest caller-requested pointers, in ascending sequence order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableEventSeed {
    pub published: u64,
    pub tail_entry_ids: Vec<i64>,
}

/// Filter set for [`Journal::query`]. `Default` matches everything with the
/// default limit.
#[derive(Default)]
pub struct JournalQuery {
    /// Only entries with `ts_ns >= since_ts_ns`.
    pub since_ts_ns: Option<i64>,
    /// Only entries recorded for this exact named session.
    pub session: Option<String>,
    /// Only entries whose `src`'s first whitespace-separated word equals this.
    pub head: Option<String>,
    /// Only entries recorded by this principal.
    pub principal: Option<String>,
    /// Only entries with this semantic role.
    pub kind: Option<EntryKind>,
    /// Only finished entries with this success verdict (unfinished entries have
    /// `NULL` ok and never match).
    pub ok: Option<bool>,
    /// Maximum rows returned; `0` means the default of 100.
    pub limit: usize,
}

impl Journal {
    /// Count coarse exec-level journal events for one exact owner and return
    /// only the newest `tail_limit` pointers.
    pub fn journal_event_seed(
        &self,
        principal: &str,
        session: &str,
        tail_limit: usize,
    ) -> rusqlite::Result<DurableEventSeed> {
        let published: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM entry
              WHERE principal = ?1 AND session = ?2 AND kind = 'exec'",
            rusqlite::params![principal, session],
            |row| row.get(0),
        )?;
        let mut stmt = self.conn.prepare(
            "SELECT id FROM entry
              WHERE principal = ?1 AND session = ?2 AND kind = 'exec'
              ORDER BY id DESC LIMIT ?3",
        )?;
        let mut tail_entry_ids = stmt
            .query_map(
                rusqlite::params![principal, session, sql_i64_from_usize(tail_limit)?],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        tail_entry_ids.reverse();
        Ok(DurableEventSeed {
            published: count_as_u64(published)?,
            tail_entry_ids,
        })
    }

    /// Resolve an exact half-open sequence page for one owner's durable
    /// `journal` channel without materializing preceding history.
    pub fn journal_event_entry_ids(
        &self,
        principal: &str,
        session: &str,
        start_seq: u64,
        limit: usize,
    ) -> rusqlite::Result<Vec<i64>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT id FROM entry
              WHERE principal = ?1 AND session = ?2 AND kind = 'exec'
              ORDER BY id ASC LIMIT ?3 OFFSET ?4",
        )?;
        stmt.query_map(
            rusqlite::params![
                principal,
                session,
                sql_i64_from_usize(limit)?,
                sql_i64_from_u64(start_seq)?
            ],
            |row| row.get(0),
        )?
        .collect()
    }

    /// Whether `hash` is linked from an output row owned by the exact
    /// principal-private session. This is the authorization lookup used before
    /// serving CAS bytes over `blob.get`; it avoids materializing an owner's
    /// entire journal merely to check one content address.
    pub fn output_owned_by(
        &self,
        hash: &str,
        session: &str,
        principal: &str,
    ) -> rusqlite::Result<bool> {
        let Ok(hash) = hex_bytes(hash) else {
            return Ok(false);
        };
        self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1
                   FROM output o
                   JOIN entry e ON e.id = o.entry_id
                  WHERE o.hash = ?1 AND e.session = ?2 AND e.principal = ?3
             )",
            rusqlite::params![hash, session, principal],
            |row| row.get(0),
        )
    }

    /// Query entries newest-first with the filters in `q`, outputs joined in.
    ///
    /// `limit == 0` means the default of 100. The `head` filter matches entries
    /// whose `src`'s first whitespace-separated word equals `head` exactly.
    pub fn query(&self, q: &JournalQuery) -> rusqlite::Result<Vec<EntryRow>> {
        self.query_inner(q, None)
    }

    /// Query entries newest-first while retaining only rows whose structured
    /// effects contain `wanted`. Filtering happens as SQLite rows are stepped,
    /// before any row is retained, and stops once `q.limit` matches have been
    /// collected. This is the bounded history-CLI counterpart to [`Self::query`].
    pub fn query_effect_contains(
        &self,
        q: &JournalQuery,
        wanted: &str,
    ) -> rusqlite::Result<Vec<EntryRow>> {
        self.query_inner(q, Some(wanted))
    }

    fn query_inner(
        &self,
        q: &JournalQuery,
        effect_contains: Option<&str>,
    ) -> rusqlite::Result<Vec<EntryRow>> {
        let limit = if q.limit == 0 {
            DEFAULT_QUERY_LIMIT
        } else {
            q.limit
        };
        let limit_i64 = limit as i64;

        let mut sql = String::from(
            "SELECT id, session, principal, ts, dur_ns, cwd, src, ast, effects, status, ok, opaque,
                    kind, parent_id
             FROM entry",
        );
        let mut clauses: Vec<&str> = Vec::new();
        let mut params: Vec<&dyn ToSql> = Vec::new();
        if let Some(ts) = q.since_ts_ns.as_ref() {
            clauses.push("ts >= ?");
            params.push(ts);
        }
        if let Some(session) = q.session.as_ref() {
            clauses.push("session = ?");
            params.push(session);
        }
        if let Some(principal) = q.principal.as_ref() {
            clauses.push("principal = ?");
            params.push(principal);
        }
        if let Some(kind) = q.kind.as_ref() {
            clauses.push("kind = ?");
            params.push(kind);
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
        // The head/effect filters are applied while stepping rows (SQL cannot
        // cheaply express their exact semantics), so SQL LIMIT is usable only
        // when neither filter is present.
        if q.head.is_none() && effect_contains.is_none() {
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
            let effects_json: String = row.get(8)?;
            if let Some(wanted) = effect_contains
                && !serde_json::from_str::<serde_json::Value>(&effects_json)
                    .ok()
                    .is_some_and(|value| effect_matches(&value, wanted))
            {
                continue;
            }
            out.push(EntryRow {
                id: row.get(0)?,
                kind: row.get(12)?,
                parent_id: row.get(13)?,
                session: row.get(1)?,
                principal: row.get(2)?,
                ts_ns: row.get(3)?,
                dur_ns: row.get(4)?,
                cwd: row.get(5)?,
                src,
                ast_json: row.get(7)?,
                effects_json,
                status: row.get(9)?,
                ok: row.get(10)?,
                opaque: row.get(11)?,
                outputs: Vec::new(),
            });
            if out.len() >= limit {
                break;
            }
        }
        self.join_outputs(&mut out)?;
        Ok(out)
    }

    /// Fetch entries for a specific, caller-known set of ids, in the EXACT
    /// order requested (not database order) — the cold-replay counterpart to
    /// [`Journal::query`]'s filtered newest-first scan. A caller that already
    /// knows precisely which rows it needs (e.g. `shoal-kernel`'s
    /// `journal`-channel replay resolving a seq→`entry_id` index) uses this
    /// instead of a wide query + in-memory filter, so the cold path pulls
    /// only the needed rows. Ids not present in the store are simply absent
    /// from the result — never an error, and never a placeholder row.
    pub fn entries_by_id(&self, ids: &[i64]) -> rusqlite::Result<Vec<EntryRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, session, principal, ts, dur_ns, cwd, src, ast, effects, status, ok, opaque,
                    kind, parent_id
             FROM entry WHERE id IN ({placeholders})"
        );
        let params: Vec<&dyn ToSql> = ids.iter().map(|id| id as &dyn ToSql).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params.as_slice())?;
        let mut by_id: HashMap<i64, EntryRow> = HashMap::new();
        while let Some(row) = rows.next()? {
            let entry = EntryRow {
                id: row.get(0)?,
                kind: row.get(12)?,
                parent_id: row.get(13)?,
                session: row.get(1)?,
                principal: row.get(2)?,
                ts_ns: row.get(3)?,
                dur_ns: row.get(4)?,
                cwd: row.get(5)?,
                src: row.get(6)?,
                ast_json: row.get(7)?,
                effects_json: row.get(8)?,
                status: row.get(9)?,
                ok: row.get(10)?,
                opaque: row.get(11)?,
                outputs: Vec::new(),
            };
            by_id.insert(entry.id, entry);
        }
        let mut ordered: Vec<EntryRow> = ids.iter().filter_map(|id| by_id.remove(id)).collect();
        self.join_outputs(&mut ordered)?;
        Ok(ordered)
    }

    /// Join each entry's `output` rows in, in recording order — the shared
    /// tail of both [`Journal::query`] and [`Journal::entries_by_id`].
    fn join_outputs(&self, entries: &mut [EntryRow]) -> rusqlite::Result<()> {
        let mut out_stmt = self.conn.prepare(
            "SELECT kind, hash, len, meta FROM output WHERE entry_id = ?1 ORDER BY rowid",
        )?;
        for entry in entries {
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
                        hash: hash_string(&raw, 1)?,
                        len: r.get(2)?,
                        meta,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(())
    }
}

fn effect_matches(value: &serde_json::Value, wanted: &str) -> bool {
    match value {
        serde_json::Value::Array(values) => {
            values.iter().any(|value| effect_matches(value, wanted))
        }
        serde_json::Value::String(value) => value.contains(wanted),
        serde_json::Value::Object(fields) => fields
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind.contains(wanted)),
        _ => false,
    }
}
