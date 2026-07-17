//! Durable storage for `session.transcript` channel events. See
//! `site/content/internals/kernel-protocol.md` and
//! `site/content/internals/journal-storage-reference.md`. This is the schema
//! follow-up required for durable transcript replay.
//!
//! The `journal` channel's replay (`shoal-kernel`'s `EventBus::journal_index`
//! and `reconstruct_journal_events`) never needed a new table: its payload
//! (`{entry_id, head, ok, principal}`) is rebuilt entirely from pre-existing
//! `entry` columns, so only an in-memory seq→`entry_id` pointer was needed.
//! The `session.transcript` payload (`{n, ref, summary:{type, ok?, cmd?,
//! n?}}`) has no such home — it is derived from the evaluated `Value`, which
//! the journal never durably stores in that shape — so there is nowhere to
//! reconstruct it from without persisting it directly.
//!
//! This module gives it that home, keyed by the same journal `entry_id` the
//! sibling `journal` event for the exact same exec already carries: a
//! `session.transcript` event fires at most once per entry, immediately
//! after the `journal` event, only on the successful-exec path
//! (`shoal-kernel/src/handlers_exec.rs`) — so `entry_id` is already a unique,
//! durable key for it, with no separate id space required. The row stores
//! the live event's `ts` and its exact `$`-tagged payload JSON verbatim, so
//! reconstruction re-wraps them into an `Event`, never re-derives them from
//! (possibly lossy) other columns.

use rusqlite::{ToSql, params};
use std::collections::HashMap;

use crate::{DurableEventSeed, Journal};

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

/// One durably-stored `session.transcript` channel event, keyed by the
/// journal `entry_id` of the exec that produced it.
#[derive(Debug, Clone)]
pub struct TranscriptEventRow {
    /// The journal entry this transcript event was published for — the same
    /// `entry_id` the sibling `journal` channel event carries.
    pub entry_id: i64,
    /// Wall-clock instant (ns since the Unix epoch) the live event fired,
    /// stored verbatim so a reconstructed event's `ts` is exact rather than
    /// approximated from the entry's start+duration the way the `journal`
    /// channel's reconstruction is.
    pub ts_ns: i64,
    /// The exact `$`-tagged `{n, ref, summary}` JSON the live event carried.
    pub payload_json: String,
}

impl Journal {
    /// Count and retain only the newest transcript-event pointers for one
    /// exact owner. The join supplies principal/session isolation while the
    /// transcript table itself is the exact channel membership set.
    pub fn transcript_event_seed(
        &self,
        principal: &str,
        session: &str,
        tail_limit: usize,
    ) -> rusqlite::Result<DurableEventSeed> {
        let published: i64 = self.conn.query_row(
            "SELECT COUNT(*)
               FROM transcript_event t JOIN entry e ON e.id = t.entry_id
              WHERE e.principal = ?1 AND e.session = ?2",
            params![principal, session],
            |row| row.get(0),
        )?;
        let mut stmt = self.conn.prepare(
            "SELECT t.entry_id
               FROM transcript_event t JOIN entry e ON e.id = t.entry_id
              WHERE e.principal = ?1 AND e.session = ?2
              ORDER BY t.entry_id DESC LIMIT ?3",
        )?;
        let mut tail_entry_ids = stmt
            .query_map(
                params![principal, session, sql_i64_from_usize(tail_limit)?],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        tail_entry_ids.reverse();
        Ok(DurableEventSeed {
            published: count_as_u64(published)?,
            tail_entry_ids,
        })
    }

    /// Resolve an exact forward sequence page for one owner's transcript
    /// channel. OFFSET is the channel sequence, not the journal row id.
    pub fn transcript_event_entry_ids(
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
            "SELECT t.entry_id
               FROM transcript_event t JOIN entry e ON e.id = t.entry_id
              WHERE e.principal = ?1 AND e.session = ?2
              ORDER BY t.entry_id ASC LIMIT ?3 OFFSET ?4",
        )?;
        stmt.query_map(
            params![
                principal,
                session,
                sql_i64_from_usize(limit)?,
                sql_i64_from_u64(start_seq)?
            ],
            |row| row.get(0),
        )?
        .collect()
    }

    /// Persist a `session.transcript` event's payload for `entry_id`
    /// (site/content/internals/kernel-protocol.md). Called once, right after the corresponding
    /// `journal` event is published for the same entry — only the
    /// successful-exec path ever produces a transcript event, so this is
    /// never called twice for the same `entry_id`.
    pub fn record_transcript_event(
        &self,
        entry_id: i64,
        ts_ns: i64,
        payload_json: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO transcript_event (entry_id, ts, payload) VALUES (?1, ?2, ?3)",
            params![entry_id, ts_ns, payload_json],
        )?;
        Ok(())
    }

    /// Fetch persisted transcript-event rows for the given entry ids, in the
    /// exact order requested (mirrors [`Journal::entries_by_id`]'s
    /// order-preserving, missing-ids-skipped contract). This is the cold
    /// replay path's targeted lookup: `shoal-kernel`'s
    /// `session.transcript`-channel reconstruction resolves a seq→`entry_id`
    /// index straight to the rows it needs, rather than scanning every row
    /// ever recorded.
    pub fn transcript_events_by_entry(
        &self,
        entry_ids: &[i64],
    ) -> rusqlite::Result<Vec<TranscriptEventRow>> {
        if entry_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat_n("?", entry_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT entry_id, ts, payload FROM transcript_event WHERE entry_id IN ({placeholders})"
        );
        let params: Vec<&dyn ToSql> = entry_ids.iter().map(|id| id as &dyn ToSql).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params.as_slice())?;
        let mut by_id: HashMap<i64, TranscriptEventRow> = HashMap::new();
        while let Some(row) = rows.next()? {
            let r = TranscriptEventRow {
                entry_id: row.get(0)?,
                ts_ns: row.get(1)?,
                payload_json: row.get(2)?,
            };
            by_id.insert(r.entry_id, r);
        }
        Ok(entry_ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }
}
