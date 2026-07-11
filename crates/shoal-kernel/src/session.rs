//! Session state (`Session`, the attached-connection `Attachment`) plus the
//! `session.attach` dispatch handler. Split out of `lib.rs` (docs/ROADMAP.md
//! wave R4): pure mechanical move, zero wire/behavior change.
use super::*;

#[derive(Clone)]
pub(crate) struct Attachment {
    pub(crate) session: Arc<Session>,
    pub(crate) principal: String,
}

pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) evaluator: Mutex<Evaluator>,
    pub(crate) transcript: Mutex<HashMap<Ref, Value>>,
    pub(crate) client_it: Mutex<HashMap<u64, Ref>>,
    pub(crate) next_value: AtomicU64,
}

impl Kernel {
    pub(crate) fn session(&self, name: &str) -> io::Result<Arc<Session>> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(name) {
            return Ok(session.clone());
        }
        let cwd = std::env::current_dir()?;
        let session = Arc::new(Session {
            id: name.into(),
            evaluator: Mutex::new(Evaluator::new(cwd)),
            transcript: Mutex::new(HashMap::new()),
            client_it: Mutex::new(HashMap::new()),
            next_value: AtomicU64::new(1),
        });
        sessions.insert(name.into(), session.clone());
        Ok(session)
    }

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
}
