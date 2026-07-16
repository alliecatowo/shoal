//! `dispatch` handlers for value/journal queries: `value.get`,
//! `journal.query`. The `events.*` handlers live in `eventbus.rs` alongside
//! the `EventBus` they operate on. Split out of `lib.rs`'s dispatch match
//! (docs/ROADMAP.md wave R4): pure mechanical move, zero wire/behavior change.
use super::*;

/// Map a §317 CAS-backed bytes resolution failure — a missing or corrupt blob
/// surfaced when `value.get` materializes an elided `CasBytes` ref under a
/// `slice`/`format=raw` ask — to a wire error that names the ref, so an agent
/// fetching the content gets a clear reason instead of a bare code.
fn cas_resolve_error(r#ref: &Ref, err: shoal_value::ErrorVal) -> RpcError {
    RpcError {
        code: UNKNOWN_REF,
        message: err.msg,
        data: Some(json!({"ref": r#ref, "code": err.code})),
    }
}

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
            code: UNKNOWN_REF,
            message: "unknown value ref".into(),
            data: None,
        })?;
        let resolved = match params.path.as_deref() {
            Some(path) if !path.is_empty() => {
                resolve_value_path(value, path).map_err(|message| RpcError {
                    code: BAD_PATH_OR_SLICE,
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
            // A table IS a list<record> semantically (TDD §4.1) — slicing it
            // used to silently no-op, returning the WHOLE table as if the
            // slice had been applied.
            (Some([start, end]), Value::Table(rows)) => {
                let start = start.min(rows.len());
                let end = end.max(start).min(rows.len());
                Value::Table(rows[start..end].to_vec())
            }
            // Str/Bytes slice by char/byte — the same targeted-drilldown ask.
            (Some([start, end]), Value::Str(s)) => {
                let chars: Vec<char> = s.chars().collect();
                let start = start.min(chars.len());
                let end = end.max(start).min(chars.len());
                Value::Str(chars[start..end].iter().collect())
            }
            (Some([start, end]), Value::Bytes(b)) => {
                let start = start.min(b.len());
                let end = end.max(start).min(b.len());
                Value::Bytes(std::sync::Arc::new(b[start..end].to_vec()))
            }
            // §317: a slice of a CAS-backed bytes ref RESOLVES it. Slicing is
            // an explicit "give me these bytes" ask, so materialize the full
            // content from the CAS (through the value's own loader — the same
            // `BytesLoad`/`Cas` seam the in-language path uses) and slice it.
            // A small slice then travels inline; a slice that is itself still
            // huge re-elides at the wall below, exactly like a plain `bytes`.
            (Some([start, end]), Value::CasBytes(c)) => {
                let full = c
                    .resolve()
                    .map_err(|e| cas_resolve_error(&params.r#ref, e))?;
                let start = start.min(full.len());
                let end = end.max(start).min(full.len());
                Value::Bytes(std::sync::Arc::new(full[start..end].to_vec()))
            }
            // Unordered/scalar values: a slice is a caller error — say so
            // instead of silently returning the unsliced value.
            (Some(_), other) => {
                return Err(RpcError {
                    code: BAD_PATH_OR_SLICE,
                    message: format!("cannot slice a {}", other.type_name()),
                    data: Some(json!({"ref":params.r#ref})),
                });
            }
            (None, other) => other,
        };
        // `format` (AGENT-SURFACE §1): "json" (default) → $-tagged wire value;
        // "render" → the human render string; "raw" → str verbatim / bytes
        // base64 (anything else has no raw byte form — say so).
        match params.format.as_deref() {
            None | Some("json") => {
                let budget = ElideBudget::from_spec(params.elide.as_ref());
                let uri = short_ref_to_uri(&params.r#ref, params.path.as_deref());
                let wire = elide_wire_value(&sliced, &uri, &budget);
                encode(json!({"ref":params.r#ref,"value":wire}))
            }
            Some("render") => {
                let render = shoal_value::render::render_block(&sliced, 80);
                // Same hard cap as MCP's content[0].text (AGENT-SURFACE §3):
                // `format=render` must not be a way to bypass the elision
                // wall by asking for the human render instead of the value.
                let uri = short_ref_to_uri(&params.r#ref, params.path.as_deref());
                encode(json!({
                    "ref": params.r#ref,
                    "render": bound_render(render, &uri, !attachment.tty),
                }))
            }
            Some("raw") => {
                let raw = match &sliced {
                    Value::Str(s) => json!({"ref":params.r#ref,"raw":s}),
                    Value::Bytes(b) => json!({
                        "ref": params.r#ref,
                        "raw_base64": base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &***b,
                        ),
                    }),
                    // §317: `format=raw` on a CAS-backed bytes ref resolves it —
                    // materialize the full content from the CAS and hand back its
                    // base64, exactly as for a resident `bytes`. (An unsliced
                    // CasBytes only reaches here under `format=raw`; the default
                    // `format=json` path still elides it to a ref, above.)
                    Value::CasBytes(c) => {
                        let full = c
                            .resolve()
                            .map_err(|e| cas_resolve_error(&params.r#ref, e))?;
                        json!({
                            "ref": params.r#ref,
                            "raw_base64": base64::Engine::encode(
                                &base64::engine::general_purpose::STANDARD,
                                &full,
                            ),
                        })
                    }
                    other => {
                        return Err(RpcError {
                            code: BAD_PATH_OR_SLICE,
                            message: format!(
                                "format \"raw\" applies to str/bytes, not {}",
                                other.type_name()
                            ),
                            data: None,
                        });
                    }
                };
                encode(raw)
            }
            Some(other) => Err(RpcError {
                code: INVALID_PARAMS,
                message: format!("format must be json, render, or raw (got {other:?})"),
                data: None,
            }),
        }
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
