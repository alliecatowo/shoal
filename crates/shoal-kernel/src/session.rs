//! Session state (`Session`, the attached-connection `Attachment`) plus the
//! `session.attach` dispatch handler. Split out of `lib.rs` (docs/ROADMAP.md
//! wave R4): pure mechanical move, zero wire/behavior change.
use super::*;

#[derive(Clone)]
pub(crate) struct Attachment {
    pub(crate) session: Arc<Session>,
    pub(crate) principal: String,
    /// Whether the attaching client declared itself a real interactive
    /// terminal (`session.attach`'s `client.tty`). Every client this
    /// codebase actually ships today (`shoal-mcp`, the test harness) attaches
    /// with `tty:false` — `shoal` (the REPL binary) never goes through the
    /// kernel at all (CLAUDE.md: "shoal never depends on or spawns
    /// shoal-kernel"), so this is currently always `false` in practice. It
    /// exists so kernel-side rendering can tell a genuine future interactive
    /// kernel-hosted client (colors wanted) apart from a headless/MCP one
    /// (colors are agent-hostile noise) — see `bound_render`'s `strip` param.
    pub(crate) tty: bool,
}

pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) evaluator: Mutex<Evaluator>,
    /// The evaluator's in-language event bus, cached so wire publishes can
    /// inject into it without taking the evaluator lock (a long-running exec
    /// must not stall `events.publish`).
    pub(crate) lang_bus: Arc<shoal_eval::EventBus>,
    pub(crate) transcript: Mutex<HashMap<Ref, Value>>,
    pub(crate) client_it: Mutex<HashMap<u64, Ref>>,
    pub(crate) next_value: AtomicU64,
}

impl Kernel {
    /// Get-or-create the named session. `principal` is only consulted the
    /// FIRST time this session name is created (an already-cached session
    /// keeps whatever `Evaluator` it was built with, journal included) — its
    /// only caller, `handle_session_attach`, always knows `who` before
    /// calling this.
    pub(crate) fn session(&self, name: &str, principal: &str) -> io::Result<Arc<Session>> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(name) {
            return Ok(session.clone());
        }
        let cwd = std::env::current_dir()?;
        let mut evaluator = Evaluator::new(cwd);
        // Long-lived agent/interactive sessions build up `j`/`jump` directory
        // history against the shared per-user store, same as the REPL (frecency
        // recording is best-effort and never fails a cd).
        evaluator.open_default_jump_history();
        // Install a command journal on the session's own evaluator (TDD §9),
        // mirroring `crates/shoal/src/repl.rs`'s `set_journal` call: without
        // this, the evaluator's per-statement journal integration
        // (`journal_begin_stmt`/`stmt_source` in `shoal-eval/src/journal.rs`)
        // never runs, so the in-language `history`/`journal` builtin is inert
        // in every kernel session — even though `handle_exec` already
        // records the same statement in the kernel's own separate,
        // coarser exec-level journal (`self.journal` above, unaffected by
        // this change).
        //
        // `Journal::open` here opens a SECOND, independent handle onto the
        // exact same on-disk state dir `self.journal` was opened against
        // (SQLite/WAL supports concurrent handles on one store fine) — never
        // a divergent path: `self.state_dir` is `Some` only when this
        // `Kernel` was itself built via `Kernel::open`/`open_with_policy`
        // against that same dir. An ephemeral in-memory kernel
        // (`Kernel::new`/`with_policy`, what most unit tests use) has no
        // on-disk state dir at all, so this is skipped entirely there,
        // exactly as before this change.
        //
        // Best-effort, like the REPL: a real open failure (permissions, a
        // corrupt store, …) must never fail session creation — the session
        // still works, just with the in-language history/journal builtin
        // disabled, the same way an interactive REPL degrades when its own
        // journal can't be opened.
        if let Some(state_dir) = &self.state_dir {
            match Journal::open(state_dir) {
                Ok(journal) => evaluator.set_journal(journal, name, principal),
                Err(error) => {
                    eprintln!(
                        "shoal-kernel: warning: journal unavailable for session {name:?} \
                         ({error}); in-language history/journal disabled this session"
                    );
                }
            }
        }
        // Bridge in-language channels onto the kernel wire bus (AGENT-SURFACE
        // §4's "one substrate"): `channel("user.x").emit(v)` in evaluated
        // source reaches `events.subscribe`/`resources/subscribe` clients.
        // The evaluator forwards only `user.*` (its own guard), so language
        // code cannot spoof kernel-owned semantic channels.
        let wire_bus = self.events.clone();
        evaluator.set_event_forwarder(Box::new(move |channel, payload| {
            let json = serde_json::to_value(crate::wire::wire_value(payload))
                .unwrap_or(serde_json::Value::Null);
            wire_bus.publish(channel, json);
        }));
        let lang_bus = evaluator.event_bus();
        let session = Arc::new(Session {
            id: name.into(),
            evaluator: Mutex::new(evaluator),
            lang_bus,
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
        let tty = params.client.tty;
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
        let session = self.session(&name, &who).map_err(internal)?;
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
            tty,
        });
        // TDD §8 tier honesty: report the REAL strongest OS backend
        // available on this host (Landlock → A, Seatbelt → C, else
        // advisory D), and whether this principal's spawns will
        // *actually* be confined — true only when a genuine OS backend
        // exists AND this principal's policy resolves to a real sandbox
        // (a scoped agent), never for the default-permissive human.
        let status = EnforcementStatus::detect();
        let tier = tier_letter(status.available_tier);
        let caps_enforced = self.caps_enforced_for(&who);
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
