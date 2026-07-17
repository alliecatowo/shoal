//! `dispatch` handlers for read-only introspection: `parse`, `complete`,
//! `explain`, `blob.get`. `session.attach` itself lives in `session.rs`
//! alongside the `Session`/`Attachment` state it populates. Split out of
//! `lib.rs`'s dispatch match. Wire behavior is documented in
//! `site/content/internals/kernel-protocol.md`.
use super::*;

impl Kernel {
    pub(crate) fn handle_parse(self: &Arc<Self>, params: Json) -> Result<Json, RpcError> {
        let params: ParseParams = decode(params)?;
        let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
            code: PARSE_ERROR,
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
                code: PARSE_ERROR,
                message: e.msg,
                data: Some(json!({"span": e.span, "hint": e.hint})),
            })?
        } else if let Some(ast_json) = p.ast {
            serde_json::from_value(ast_json).map_err(|e| RpcError {
                code: INVALID_PARAMS,
                message: format!("invalid ast: {e}"),
                data: None,
            })?
        } else {
            return Err(RpcError {
                code: INVALID_PARAMS,
                message: "explain requires src or ast".into(),
                data: None,
            });
        };
        let ast_json = serde_json::to_string(&ast).map_err(internal)?;
        let plan = {
            let mut evaluator = session.lock_evaluator()?;
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
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let params: BlobGetParams = decode(params)?;
        let hash = params.hash;
        let journal = self
            .journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))?;
        let owned = journal
            .output_owned_by(&hash, &attachment.session.id, &attachment.principal)
            .map_err(internal)?;
        if !owned {
            return Err(RpcError {
                code: UNKNOWN_REF,
                message: "unknown value hash".into(),
                data: None,
            });
        }
        let requested_offset = params.offset.unwrap_or(0);
        let requested_length = params.length.unwrap_or(RAW_PAGE_MAX_BYTES as u64);
        let effective_length = requested_length.min(RAW_PAGE_MAX_BYTES as u64) as usize;
        let cached = journal
            .cached_blob_range(&hash, requested_offset, effective_length)
            .map_err(internal)?;
        let (total_len, blob) = match cached {
            Some(page) => page,
            None => {
                self.reserve_blob_decompression(&attachment.session)?;
                journal
                    .read_blob_range(&hash, requested_offset, effective_length)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError {
                        code: UNKNOWN_REF,
                        message: "unknown value hash".into(),
                        data: None,
                    })?
            }
        };
        let offset = requested_offset.min(total_len);
        let returned_len = blob.len() as u64;
        let next = offset.saturating_add(returned_len).min(total_len);
        let requested_end = offset.saturating_add(requested_length).min(total_len);
        let page = json!({
            "total_len": total_len,
            "offset": offset,
            "returned_len": returned_len,
            "content_bytes": blob.len(),
            "next_offset": (next < total_len).then_some(next),
            "done": next >= total_len,
            "truncated": next < total_len,
            "request_truncated": next < requested_end,
            "unit": "byte",
            "max_content_bytes": RAW_PAGE_MAX_BYTES,
        });

        // Preserve the historical structured response for a complete small
        // omitted-range request. Larger blobs and every explicit range return
        // a byte page; neither path ever allocates more than the page wall.
        if params.offset.is_none() && params.length.is_none() && next == total_len {
            let value = serde_json::from_slice::<Json>(&blob).unwrap_or_else(|_| {
                json!({
                    "$": "bytes",
                    "len": blob.len(),
                    "v": base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD, &blob),
                })
            });
            encode(json!({"hash": hash, "value": value, "page": page}))
        } else {
            encode(json!({
                "hash": hash,
                "encoding": "base64",
                "raw_base64": base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &blob,
                ),
                "page": page,
            }))
        }
    }
}
