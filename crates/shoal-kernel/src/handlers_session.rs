//! `dispatch` handlers for session lifecycle + read-only introspection:
//! `session.attach`, `parse`, `complete`, `explain`, `blob.get`. Split out of
//! `lib.rs`'s dispatch match (docs/ROADMAP.md wave R4): pure mechanical move,
//! zero wire/behavior change.
use super::*;

impl Kernel {
    pub(crate) fn handle_session_attach(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let params: AttachParams = decode(params)?;
        let (who, token_caps, profile) = if let Some(token) = params.token {
            let auth = self.auth.as_ref().ok_or_else(|| RpcError {
                code: -32030,
                message: "bearer tokens unavailable in ephemeral kernel".into(),
                data: None,
            })?;
            let meta = auth
                .lock()
                .unwrap()
                .validate(&token)
                .ok_or_else(|| RpcError {
                    code: -32030,
                    message: "invalid, expired, or revoked bearer token".into(),
                    data: None,
                })?;
            (meta.principal, meta.caps, meta.profile)
        } else {
            (principal(), vec![], "local-human".into())
        };
        let name = params.session.unwrap_or_else(|| "default".into());
        let session = self.session(&name).map_err(internal)?;
        let cwd = session
            .evaluator
            .lock()
            .unwrap()
            .cwd()
            .as_os_str()
            .to_owned();
        *attached = Some(Attachment {
            session,
            principal: who.clone(),
        });
        // TDD §8 tier honesty: report the REAL strongest OS backend
        // available on this host (Landlock → A, Seatbelt → C, else
        // advisory D), and whether this principal's spawns will
        // *actually* be confined — true only when a genuine OS backend
        // exists AND this principal's policy resolves to a real sandbox
        // (a scoped agent), never for the default-permissive human.
        let status = EnforcementStatus::detect();
        let tier = tier_letter(status.available_tier);
        let backend_present = matches!(
            status.available_tier,
            EnforcementTier::A | EnforcementTier::C
        );
        let caps_enforced = backend_present && self.policy.sandbox_for(&who).is_some();
        encode(AttachResult {
            session: name,
            principal: who.clone(),
            caps: json!({"enforced":caps_enforced,"tier":tier,"available_tier":tier,"policy_principal":who,"profile":profile,"token_caps":token_caps,"opaque":verdict_name(self.policy.evaluate_effect(&who, &Effect::Opaque))}),
            cwd: WirePath::encode(&cwd),
            env_hash: "local".into(),
            ast_version: AST_VERSION,
            caps_enforced,
            elide_defaults: elide_defaults_json(),
            channels: STATIC_CHANNELS.iter().map(|s| s.to_string()).collect(),
        })
    }

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
