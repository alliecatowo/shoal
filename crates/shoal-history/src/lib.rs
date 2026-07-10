//! User-facing journal/history operations, separated from presentation and CLI parsing.

use serde_json::{Value, json};
use shoal_journal::{EntryRow, GcOptions, GcReport, Journal, JournalQuery, UndoReport};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct QueryFilter {
    pub since_ns: Option<i64>,
    pub principal: Option<String>,
    pub effect: Option<String>,
    pub head: Option<String>,
    pub ok: Option<bool>,
    pub limit: usize,
}

pub fn query(journal: &Journal, filter: &QueryFilter) -> Result<Vec<EntryRow>, rusqlite::Error> {
    let requested = if filter.limit == 0 { 100 } else { filter.limit };
    let q = JournalQuery {
        since_ts_ns: filter.since_ns,
        head: filter.head.clone(),
        principal: filter.principal.clone(),
        ok: filter.ok,
        limit: if filter.effect.is_some() {
            usize::MAX
        } else {
            requested
        },
    };
    let mut rows = journal.query(&q)?;
    if let Some(effect) = &filter.effect {
        rows.retain(|r| {
            serde_json::from_str::<Value>(&r.effects_json)
                .ok()
                .is_some_and(|v| effect_matches(&v, effect))
        });
        rows.truncate(requested);
    }
    Ok(rows)
}

fn effect_matches(value: &Value, wanted: &str) -> bool {
    match value {
        Value::Array(xs) => xs.iter().any(|v| effect_matches(v, wanted)),
        Value::String(s) => s.contains(wanted),
        Value::Object(m) => m
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|s| s.contains(wanted)),
        _ => false,
    }
}

pub fn entry(journal: &Journal, id: i64) -> Result<Option<EntryRow>, rusqlite::Error> {
    Ok(journal
        .query(&JournalQuery {
            limit: usize::MAX,
            ..Default::default()
        })?
        .into_iter()
        .find(|r| r.id == id))
}

pub fn entry_json(journal: &Journal, row: &EntryRow) -> Value {
    let outputs=row.outputs.iter().map(|o|{let available=journal.read_blob(&o.hash).ok().flatten().is_some();json!({"kind":o.kind,"hash":o.hash,"stored_len":o.len,"meta":o.meta.as_ref().map(|m|json!({"truncated":m.truncated,"original_len":m.original_len,"stored_len":m.stored_len})),"available":available,"aged_out":!available})}).collect::<Vec<_>>();
    json!({"id":row.id,"session":row.session,"principal":row.principal,"ts_ns":row.ts_ns,"dur_ns":row.dur_ns,"cwd":String::from_utf8_lossy(&row.cwd),"src":row.src,"ast":serde_json::from_str::<Value>(&row.ast_json).unwrap_or(Value::String(row.ast_json.clone())),"effects":serde_json::from_str::<Value>(&row.effects_json).unwrap_or(Value::String(row.effects_json.clone())),"status":row.status,"ok":row.ok,"opaque":row.opaque,"outputs":outputs})
}

pub fn render_human(journal: &Journal, rows: &[EntryRow], verbose: bool) -> String {
    let mut out = String::new();
    for row in rows {
        let verdict = match row.ok {
            Some(true) => "ok",
            Some(false) => "failed",
            None => "unfinished",
        };
        out.push_str(&format!(
            "{}  {}  {}  {}\n",
            row.id,
            row.principal,
            verdict,
            row.src.lines().next().unwrap_or("")
        ));
        if verbose {
            for output in &row.outputs {
                let available = journal.read_blob(&output.hash).ok().flatten().is_some();
                let state = if available { "available" } else { "aged out" };
                let trunc = output
                    .meta
                    .as_ref()
                    .filter(|m| m.truncated)
                    .map(|m| format!(", truncated from {} bytes", m.original_len))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "    {}: {} bytes, {}{} [{}]\n",
                    output.kind, output.len, state, trunc, output.hash
                ));
            }
        }
    }
    out
}

pub fn gc(
    journal: &Journal,
    ttl: Option<Duration>,
    budget: Option<u64>,
    apply: bool,
) -> Result<GcReport, rusqlite::Error> {
    journal.gc(GcOptions {
        ttl,
        max_bytes: budget,
        dry_run: !apply,
    })
}
pub fn undo(
    journal: &Journal,
    id: i64,
    root: &Path,
) -> Result<UndoReport, shoal_journal::UndoError> {
    journal.undo_entry(id, root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoal_journal::{EntryRecord, JournalOptions};
    fn rec(src: &str, effects: &str) -> EntryRecord {
        EntryRecord {
            session: "s".into(),
            principal: "agent:x".into(),
            ts_ns: 10,
            cwd: b"/tmp".to_vec(),
            src: src.into(),
            ast_json: "{}".into(),
            effects_json: effects.into(),
            opaque: false,
        }
    }
    #[test]
    fn filters_effects_and_status() {
        let j = Journal::in_memory().unwrap();
        let a = j.append(&rec("git push", "[\"net.connect\"]")).unwrap();
        let b = j.append(&rec("ls", "[\"fs.read\"]")).unwrap();
        j.finish(a, Some(0), true, 1).unwrap();
        j.finish(b, Some(1), false, 1).unwrap();
        let rows = query(
            &j,
            &QueryFilter {
                effect: Some("net".into()),
                ok: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, a);
    }
    #[test]
    fn show_reports_available_and_aged_out() {
        let j = Journal::in_memory().unwrap();
        let id = j.append(&rec("echo hi", "[]")).unwrap();
        let hash = j.record_output(id, "stdout", b"hi").unwrap();
        let row = entry(&j, id).unwrap().unwrap();
        assert_eq!(entry_json(&j, &row)["outputs"][0]["available"], true);
        j.gc(GcOptions {
            ttl: Some(Duration::ZERO),
            max_bytes: Some(0),
            dry_run: false,
        })
        .unwrap();
        let row = entry(&j, id).unwrap().unwrap();
        assert_eq!(entry_json(&j, &row)["outputs"][0]["aged_out"], true);
        assert!(render_human(&j, &[row], true).contains("aged out"));
        assert!(!hash.is_empty());
    }
    #[test]
    fn truncation_surfaces_in_json() {
        let j = Journal::in_memory_with_options(JournalOptions {
            output_hard_cap: 64,
        })
        .unwrap();
        let id = j.append(&rec("loud", "[]")).unwrap();
        j.record_output(id, "stdout", &vec![0; 1000]).unwrap();
        let row = entry(&j, id).unwrap().unwrap();
        assert_eq!(
            entry_json(&j, &row)["outputs"][0]["meta"]["original_len"],
            1000
        );
    }
    #[test]
    fn gc_wrapper_defaults_to_dry_run() {
        let j = Journal::in_memory().unwrap();
        let id = j.append(&rec("x", "[]")).unwrap();
        let hash = j.record_output(id, "stdout", b"x").unwrap();
        let report = gc(&j, Some(Duration::ZERO), Some(0), false).unwrap();
        assert_eq!(report.deleted.len(), 0);
        assert!(j.read_blob(&hash).unwrap().is_some());
    }
}
