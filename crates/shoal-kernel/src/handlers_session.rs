//! `dispatch` handlers for read-only introspection: `parse`, `complete`,
//! `explain`, `blob.get`. `session.attach` itself lives in `session.rs`
//! alongside the `Session`/`Attachment` state it populates. Split out of
//! `lib.rs`'s dispatch match (docs/ROADMAP.md wave R4): pure mechanical move,
//! zero wire/behavior change.
use super::*;

impl Kernel {
    pub(crate) fn handle_parse(self: &Arc<Self>, params: Json) -> Result<Json, RpcError> {
        let params: ParseParams = decode(params)?;
        let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
            code: -32001,
            message: e.msg,
            data: Some(json!({"span":e.span,"hint":e.hint})),
        })?;
        encode(json!({"ast_version":AST_VERSION,"ast":ast}))
    }

    pub(crate) fn handle_complete(self: &Arc<Self>, params: Json) -> Result<Json, RpcError> {
        let p: CompleteParams = decode(params)?;
        let cursor = p.cursor.unwrap_or(p.src.len()).min(p.src.len());
        encode(json!({"candidates": complete_at(&p.src, cursor)}))
    }

    pub(crate) fn handle_explain(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let p: ExplainParams = decode(params)?;
        let ast = if let Some(src) = &p.src {
            shoal_syntax::parse(src).map_err(|e| RpcError {
                code: -32001,
                message: e.msg,
                data: Some(json!({"span": e.span, "hint": e.hint})),
            })?
        } else if let Some(ast_json) = p.ast {
            serde_json::from_value(ast_json).map_err(|e| RpcError {
                code: -32602,
                message: format!("invalid ast: {e}"),
                data: None,
            })?
        } else {
            return Err(RpcError {
                code: -32602,
                message: "explain requires src or ast".into(),
                data: None,
            });
        };
        let ast_json = serde_json::to_string(&ast).map_err(internal)?;
        let plan = {
            let mut evaluator = session.evaluator.lock().unwrap();
            derive_plan(&mut evaluator, &ast, &ast_json)
        };
        encode(json!({
            "ast_version": AST_VERSION,
            "ast": ast,
            "effects": plan.effects,
            "reversibility": reversibility_from_effects(&plan.effects),
            "plan_ref": plan.plan_ref,
        }))
    }

    pub(crate) fn handle_blob_get(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        attached.as_ref().ok_or_else(not_attached)?;
        let hash = params
            .get("hash")
            .and_then(Json::as_str)
            .ok_or_else(|| RpcError {
                code: -32602,
                message: "blob.get requires a hash".into(),
                data: None,
            })?
            .to_string();
        let blob = self
            .journal
            .lock()
            .unwrap()
            .read_blob(&hash)
            .map_err(internal)?
            .ok_or_else(|| RpcError {
                code: -32004,
                message: "unknown value hash".into(),
                data: None,
            })?;
        // Content-addressed value blobs are stored as their `$`-tagged
        // JSON encoding; hand it back structurally. A non-JSON blob
        // (stdout/stderr) comes back as tagged bytes.
        let value = serde_json::from_slice::<Json>(&blob).unwrap_or_else(|_| {
            json!({
                "$": "bytes",
                "len": blob.len(),
                "v": base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD, &blob),
            })
        });
        encode(json!({"hash": hash, "value": value}))
    }
}
