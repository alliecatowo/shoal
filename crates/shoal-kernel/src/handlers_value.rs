//! `dispatch` handlers for value/journal queries: `value.get`,
//! `journal.query`. The `events.*` handlers live in `eventbus.rs` alongside
//! the `EventBus` they operate on. Split out of `lib.rs`'s dispatch match
//! (docs/ROADMAP.md wave R4): pure mechanical move, zero wire/behavior change.
use super::*;

impl Kernel {
    pub(crate) fn handle_value_get(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let params: ValueGetParams = decode(params)?;
        let values = session.transcript.lock().unwrap();
        let value = values.get(&params.r#ref).ok_or_else(|| RpcError {
            code: -32004,
            message: "unknown value ref".into(),
            data: None,
        })?;
        let resolved = match params.path.as_deref() {
            Some(path) if !path.is_empty() => {
                resolve_value_path(value, path).map_err(|message| RpcError {
                    code: -32005,
                    message,
                    data: Some(json!({"ref":params.r#ref,"path":path})),
                })?
            }
            _ => value.clone(),
        };
        // Slicing is an explicit, targeted ask: apply it at the value
        // level *before* the elision check, so a small slice of a
        // huge list is never spuriously elided (and a slice that is
        // itself still huge still is).
        let sliced = match (params.slice, resolved) {
            (Some([start, end]), Value::List(items)) => {
                let start = start.min(items.len());
                let end = end.max(start).min(items.len());
                Value::List(items[start..end].to_vec())
            }
            (_, other) => other,
        };
        let budget = ElideBudget::from_spec(params.elide.as_ref());
        let uri = short_ref_to_uri(&params.r#ref, params.path.as_deref());
        let wire = elide_wire_value(&sliced, &uri, &budget);
        encode(json!({"ref":params.r#ref,"value":wire}))
    }

    pub(crate) fn handle_journal_query(self: &Arc<Self>, params: Json) -> Result<Json, RpcError> {
        let p: JournalQueryParams = decode(params)?;
        let rows = self
            .journal
            .lock()
            .unwrap()
            .query(&JournalQuery {
                since_ts_ns: p.since,
                principal: p.principal,
                head: p.head,
                ok: p.ok,
                limit: p.limit,
            })
            .map_err(internal)?;
        // The journal store filters since/principal/head/ok/limit; the
        // wire also promises `until` (upper time bound) and `effects`
        // (effect-kind subset) — kernel-side post-filters over the
        // returned rows (AGENT-SURFACE §5 / TDD §7).
        // Effect kinds are stored snake_case (`fs_delete`); agents use
        // the dotted convention (`fs.delete`). Normalize so either
        // form matches.
        let want_effects: Vec<String> = p
            .effects
            .unwrap_or_default()
            .iter()
            .map(|e| norm_effect(e))
            .collect();
        let entries: Vec<JournalEntry> = rows
            .into_iter()
            .filter(|r| p.until.is_none_or(|until| r.ts_ns <= until))
            .filter(|r| {
                want_effects.is_empty()
                    || want_effects
                        .iter()
                        .all(|want| r.effects_json.contains(want))
            })
            .map(|r| JournalEntry {
                id: r.id,
                session: r.session,
                principal: r.principal,
                ts: r.ts_ns,
                dur_ns: r.dur_ns,
                cwd: WirePath::encode(&std::ffi::OsString::from_vec(r.cwd)),
                src: r.src,
                ast: serde_json::from_str(&r.ast_json).unwrap_or(Json::Null),
                effects: serde_json::from_str(&r.effects_json).unwrap_or(Json::Null),
                status: r.status,
                ok: r.ok,
                opaque: r.opaque,
                outputs: r
                    .outputs
                    .into_iter()
                    .map(|o| JournalOutput {
                        kind: o.kind,
                        hash: o.hash,
                        len: o.len,
                    })
                    .collect(),
            })
            .collect();
        encode(entries)
    }
}
