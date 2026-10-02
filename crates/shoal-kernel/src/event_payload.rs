//! Bounded protocol event and metadata payload construction.

use super::*;

/// The kernel's default elision thresholds, advertised at attach so a client
/// knows the budget before tightening/loosening per call (site/content/internals/kernel-protocol.md).
pub(super) fn elide_defaults_json() -> Json {
    json!({
        "max_bytes": ELIDE_DEFAULT_MAX_BYTES,
        "max_rows": ELIDE_DEFAULT_MAX_ROWS,
        "max_bytes_raw": ELIDE_DEFAULT_MAX_BYTES_RAW,
        "max_items": ELIDE_DEFAULT_MAX_ITEMS,
        "hard_cap": ELIDE_HARD_CAP,
    })
}

/// The wire projection of a plan's [`ApprovalRecord`] (HR-D2), or `null` when
/// the plan has not been approved. Surfaced by `plan.get` so the full
/// requester→approver→scope→consuming-execution binding is inspectable, not
/// just an unattributed `approved: true` bit.
pub(super) fn approval_json(approval: Option<&ApprovalRecord>) -> Json {
    match approval {
        None => Json::Null,
        Some(a) => json!({
            "requester": a.requester,
            "approver": a.approver,
            "plan_ref": a.plan_ref,
            "plan_hash": a.plan_hash,
            "source_hash": a.source_hash,
            "session": a.session,
            "scope": a.scope,
            "approved_at": a.approved_at_ns,
            "grant_audit_id": a.grant_audit_id,
            "consumed_by": a.consumed_by,
        }),
    }
}

/// The `session.transcript` event payload for a new `out[n]` (see
/// `site/content/internals/kernel-protocol.md`): `{n, ref, summary:{type, ok?, cmd?, n?}}` — shape only, never payload.
pub(super) fn transcript_event(value_ref: &Ref, value: &Value) -> Json {
    let n: i64 = value_ref
        .0
        .split_once(':')
        .and_then(|(_, id)| id.parse().ok())
        .unwrap_or(0);
    let mut summary = serde_json::Map::new();
    summary.insert("type".into(), json!({"$":"str","v": value.type_name()}));
    match value {
        Value::Outcome(o) => {
            summary.insert("ok".into(), json!({"$":"bool","v": o.ok}));
            summary.insert("cmd".into(), json!({"$":"str","v": o.cmd}));
        }
        Value::Table(rows) => {
            summary.insert("n".into(), json!({"$":"int","v": rows.len()}));
        }
        Value::List(items) => {
            summary.insert("n".into(), json!({"$":"int","v": items.len()}));
        }
        _ => {}
    }
    json!({
        "$": "record",
        "v": {
            "n": {"$":"int","v": n},
            "ref": {"$":"str","v": value_ref.0},
            "summary": {"$":"record","v": summary},
        }
    })
}

/// The `approval` event payload (site/content/internals/kernel-protocol.md): `{plan_ref, effects,
/// principal, expires}`, fired once — the moment `exec {mode:"plan"}`
/// computes `Verdict::ApprovalRequired` for a newly stored plan — so a
/// SEPARATE subscriber (a human's session, a supervising agent) learns a
/// plan is stuck awaiting approval by subscribing, not by polling
/// `journal.query` or re-issuing the same plan.
///
/// `expires` is honestly `{"$":"null"}`: `StoredPlan` carries no TTL/deadline
/// field today, so there is nothing to report (same honest-omission
/// precedent as `wire::outcome_span` — report absence, never fabricate a
/// plausible-looking deadline).
pub(super) fn approval_event(plan_ref: &str, effects: &[Json], principal: &str) -> Json {
    json!({
        "$": "record",
        "v": {
            "plan_ref": {"$":"str","v": plan_ref},
            "effects": {"$":"list","v": effects},
            "principal": {"$":"str","v": principal},
            "expires": {"$":"null"},
        }
    })
}

/// The `journal` event payload (site/content/internals/kernel-protocol.md): `{entry_id, head, ok,
/// principal}`, fired once per finished journal entry (mirrors
/// `session.transcript`'s "announce right after the fact" shape — the entry
/// already exists in the journal by the time this fires). `head` is the
/// entry's leading command word (`shoal-journal`'s own `head`-filter
/// semantics: `src.split_whitespace().next()`), not a hash.
pub(super) fn journal_event(entry_id: i64, src: &str, ok: bool, principal: &str) -> Json {
    let head = src.split_whitespace().next().unwrap_or_default();
    json!({
        "$": "record",
        "v": {
            "entry_id": {"$":"int","v": entry_id},
            "head": {"$":"str","v": head},
            "ok": {"$":"bool","v": ok},
            "principal": {"$":"str","v": principal},
        }
    })
}

/// The `render` event payload (site/content/internals/kernel-protocol.md): `{ref, render}`, for a UI
/// client mirroring a session's output live without polling `value.get
/// {format:"render"}`. Fired alongside `session.transcript` for every new
/// `out[n]`, carrying the SAME bounded/ANSI-stripped render string the exec
/// response itself returns — never a second, unbounded copy.
pub(super) fn render_event(value_ref: &Ref, render: &str) -> Json {
    json!({
        "$": "record",
        "v": {
            "ref": {"$":"str","v": value_ref.0},
            "render": {"$":"str","v": render},
        }
    })
}
