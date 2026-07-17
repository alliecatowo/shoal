//! Session state (`Session`, the attached-connection `Attachment`) plus the
//! `session.attach` dispatch handler. Split out of `lib.rs` (site/content/internals/roadmap-and-priorities.md
//! `site/content/internals/change-map.md`; pure mechanical move, zero wire/behavior change.
use super::*;

/// Principal-private identity for a named evaluator session. The wire still
/// exposes only `name`; `principal` prevents two authenticated callers that
/// choose the same name from sharing mutable evaluator state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SessionKey {
    pub(crate) principal: String,
    pub(crate) name: String,
}

impl SessionKey {
    pub(crate) fn new(principal: &str, name: &str) -> Self {
        Self {
            principal: principal.to_string(),
            name: name.to_string(),
        }
    }

    pub(crate) fn owner(&self) -> OwnerKey {
        OwnerKey(self.clone())
    }
}

/// Exact owner of task/PTY/subscription quota state. Kept distinct from the
/// session registry key so ownership checks cannot accidentally regress to a
/// comparison of user-chosen session names alone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OwnerKey(pub(crate) SessionKey);

#[derive(Clone)]
pub(crate) struct Attachment {
    pub(crate) session: Arc<Session>,
    pub(crate) principal: String,
    /// Whether this authenticated attachment may approve another principal's
    /// plan. Local-human attachments are trusted; bearer attachments must opt
    /// in through the `supervisor` profile or `plan.approve` capability.
    pub(crate) can_approve: bool,
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
    /// Request-local cancellation epoch for a queued task. The task worker
    /// installs it only after acquiring the session evaluator, so a later
    /// request cannot replace its cancellation handle while it is queued.
    pub(crate) cancel_epoch: Option<shoal_exec::CancelToken>,
    /// Authenticated bearer metadata contains no secret but is sufficient to
    /// refresh revocation/expiry status against the token store before every
    /// subsequent request. Local attachment modes leave this empty.
    pub(crate) bearer: Option<TokenMeta>,
    /// Runtime security contract under which this attachment was created.
    /// Keeping it on the live attachment makes future epoch bumps fail closed
    /// instead of silently carrying old authority forward.
    pub(crate) security_epoch: u32,
}

pub(crate) struct Session {
    pub(crate) key: SessionKey,
    pub(crate) id: String,
    pub(crate) evaluator: Mutex<Evaluator>,
    /// A panic while dispatching against this session makes the evaluator's
    /// logical invariants unknowable even when Rust can recover the poisoned
    /// mutex guard. Fail closed instead of treating `PoisonError::into_inner`
    /// as validation of evaluator state.
    quarantined: AtomicBool,
    last_used_ns: AtomicI64,
    /// The evaluator's in-language event bus, cached so wire publishes can
    /// inject into it without taking the evaluator lock (a long-running exec
    /// must not stall `events.publish`).
    pub(crate) lang_bus: Arc<shoal_eval::EventBus>,
    pub(crate) transcript: Mutex<HashMap<Ref, Value>>,
    pub(crate) next_value: AtomicU64,
    stream_cursors: Mutex<HashMap<StreamCursorRef, Arc<Mutex<WireStreamCursor>>>>,
}

pub(crate) const MAX_TRANSCRIPT_PER_SESSION: usize = 4096;
pub(crate) const MAX_WIRE_STREAM_CURSORS: usize = 64;

pub(crate) struct WireStreamCursor {
    pub(crate) upstream: Option<Box<dyn shoal_value::Upstream>>,
    pub(crate) next_seq: u64,
    pub(crate) done: bool,
}

impl Session {
    pub(crate) fn touch(&self) {
        self.last_used_ns.store(now_ns(), Ordering::Relaxed);
    }

    pub(crate) fn last_used_ns(&self) -> i64 {
        self.last_used_ns.load(Ordering::Relaxed)
    }

    pub(crate) fn quarantine(&self) {
        self.quarantined.store(true, Ordering::SeqCst);
    }

    pub(crate) fn ensure_healthy(&self) -> Result<(), RpcError> {
        if self.quarantined.load(Ordering::SeqCst) {
            Err(RpcError {
                code: INTERNAL_ERROR,
                message: "session is quarantined after an internal panic".into(),
                data: Some(json!({"session": self.id, "session_quarantined": true})),
            })
        } else {
            Ok(())
        }
    }

    pub(crate) fn insert_transcript(&self, value_ref: Ref, value: Value) {
        let id = value_ref
            .0
            .split_once(':')
            .and_then(|(_, id)| id.parse::<u64>().ok());
        let mut transcript = self.transcript.lock().unwrap();
        transcript.insert(value_ref, value);
        if let Some(id) = id
            && id > MAX_TRANSCRIPT_PER_SESSION as u64
        {
            transcript.remove(&Ref::new("out", id - MAX_TRANSCRIPT_PER_SESSION as u64));
        }
    }

    /// Get or lazily claim a transcript stream's single-consumer upstream.
    /// Cursor creation is serialized under the registry lock, so concurrent
    /// first pulls cannot both consume the same `StreamVal`.
    pub(crate) fn stream_cursor(
        &self,
        cursor: &StreamCursorRef,
    ) -> Result<Arc<Mutex<WireStreamCursor>>, RpcError> {
        let mut cursors = self.stream_cursors.lock().unwrap();
        if let Some(entry) = cursors.get(cursor) {
            return Ok(entry.clone());
        }

        // Terminal cursors retain no upstream resources. Reap them at the
        // admission boundary so clients do not need to close after observing
        // `done:true` merely to make quota progress.
        if cursors.len() >= MAX_WIRE_STREAM_CURSORS {
            cursors.retain(|_, entry| !entry.lock().unwrap().done);
        }
        if cursors.len() >= MAX_WIRE_STREAM_CURSORS {
            return Err(RpcError {
                code: QUOTA_EXCEEDED,
                message: "live stream cursor quota reached".into(),
                data: Some(json!({
                    "limit": "stream_cursors_per_session",
                    "max": MAX_WIRE_STREAM_CURSORS,
                })),
            });
        }

        let stream = self.resolve_stream_value(cursor)?;
        let upstream = stream.take_upstream().map_err(stream_error)?;
        let entry = Arc::new(Mutex::new(WireStreamCursor {
            upstream: Some(upstream),
            next_seq: 0,
            done: false,
        }));
        cursors.insert(cursor.clone(), entry.clone());
        Ok(entry)
    }

    /// Explicitly release a cursor. If it has never been pulled, claim and
    /// immediately drop its upstream so source threads/resources are closed
    /// and later pulls correctly observe single consumption.
    pub(crate) fn close_stream_cursor(&self, cursor: &StreamCursorRef) -> Result<bool, RpcError> {
        if let Some(entry) = self.stream_cursors.lock().unwrap().remove(cursor) {
            // A concurrent pull may already hold this entry after releasing
            // the registry map. Wait for that bounded pull, then take/drop the
            // upstream before reporting close complete.
            let mut entry = entry.lock().unwrap();
            entry.upstream.take();
            entry.done = true;
            return Ok(true);
        }
        let stream = self.resolve_stream_value(cursor)?;
        match stream.take_upstream() {
            Ok(upstream) => {
                drop(upstream);
                Ok(true)
            }
            Err(error) if error.code == "stream_consumed" => Ok(false),
            Err(error) => Err(stream_error(error)),
        }
    }

    fn resolve_stream_value(
        &self,
        cursor: &StreamCursorRef,
    ) -> Result<shoal_value::StreamVal, RpcError> {
        let transcript = self.transcript.lock().unwrap();
        let root = transcript.get(&cursor.r#ref).ok_or_else(unknown_stream)?;
        let value = match cursor.path.as_deref() {
            Some(path) if !path.is_empty() => {
                resolve_value_path(root, path).map_err(|message| RpcError {
                    code: BAD_PATH_OR_SLICE,
                    message,
                    data: Some(json!({"ref":cursor.r#ref,"path":path})),
                })?
            }
            _ => root.clone(),
        };
        match value {
            Value::Stream(stream) => Ok(stream),
            other => Err(RpcError {
                code: BAD_PATH_OR_SLICE,
                message: format!("stream cursor addresses a {}", other.type_name()),
                data: Some(json!({"ref":cursor.r#ref,"path":cursor.path})),
            }),
        }
    }
}

fn unknown_stream() -> RpcError {
    RpcError {
        code: UNKNOWN_REF,
        message: "unknown stream cursor".into(),
        data: None,
    }
}

fn stream_error(error: shoal_value::ErrorVal) -> RpcError {
    RpcError {
        code: RAISED,
        message: error.msg.clone(),
        data: Some(json!({
            "code": error.code,
            "span": error.span,
            "hint": error.hint,
            "status": error.status,
            "stderr": error.stderr,
        })),
    }
}

impl Kernel {
    /// Get-or-create the principal-private named session.
    pub(crate) fn session(&self, name: &str, principal: &str) -> Result<Arc<Session>, RpcError> {
        let key = SessionKey::new(principal, name);
        self.sessions.get_or_try_insert_with(
            key.clone(),
            || {
                let cwd = std::env::current_dir().map_err(internal)?;
                let mut evaluator = Evaluator::new(cwd);
                // Long-lived agent/interactive sessions build up `j`/`jump` directory
                // history against the shared per-user store, same as the REPL (frecency
                // recording is best-effort and never fails a cd).
                evaluator.open_default_jump_history();
                // Install a command journal on the session's own evaluator (site/content/internals/language-conformance-contract.md),
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
                // Bridge in-language channels onto the kernel wire bus (see
                // `site/content/internals/kernel-protocol.md`): `channel("user.x").emit(v)` in evaluated
                // source reaches `events.subscribe`/`resources/subscribe` clients.
                // The evaluator forwards only `user.*` (its own guard), so language
                // code cannot spoof kernel-owned semantic channels.
                let wire_bus = self.events.clone();
                let wire_owner = key.owner();
                evaluator.set_event_forwarder(Box::new(move |channel, payload| {
                    let json = serde_json::to_value(crate::wire::wire_value(payload))
                        .unwrap_or(serde_json::Value::Null);
                    wire_bus.publish(&wire_owner, channel, json);
                }));
                let lang_bus = evaluator.event_bus();
                Ok(Arc::new(Session {
                    key: key.clone(),
                    id: name.into(),
                    evaluator: Mutex::new(evaluator),
                    quarantined: AtomicBool::new(false),
                    last_used_ns: AtomicI64::new(now_ns()),
                    lang_bus,
                    transcript: Mutex::new(HashMap::new()),
                    next_value: AtomicU64::new(1),
                    stream_cursors: Mutex::new(HashMap::new()),
                }))
            },
            |owner| {
                self.events.remove_owner(owner);
                self.tasks.remove_terminal_owner(owner);
                self.ptys.remove_terminal_owner(owner);
                self.plans.remove_owner(owner);
            },
        )
    }

    pub(crate) fn handle_session_attach(
        self: &Arc<Self>,
        params: Json,
        client: u64,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let local_auth = match params.get("local_auth") {
            Some(value) => Some(
                serde_json::from_value::<LocalAuthMode>(value.clone()).map_err(|error| {
                    RpcError {
                        code: INVALID_PARAMS,
                        message: format!("invalid local_auth: {error}"),
                        data: None,
                    }
                })?,
            ),
            None => None,
        };
        let params: AttachParams = decode(params)?;
        let tty = params.client.tty;
        if params.token.is_some() && local_auth.is_some() {
            return Err(RpcError {
                code: INVALID_PARAMS,
                message: "token and local_auth are mutually exclusive authentication modes".into(),
                data: None,
            });
        }
        let (who, token_caps, profile, local_human, auth_mode, bearer) =
            if let Some(token) = params.token {
                let auth = self.auth.as_ref().ok_or_else(|| RpcError {
                    code: AUTH_FAILED,
                    message: "bearer tokens unavailable in ephemeral kernel".into(),
                    data: None,
                })?;
                let meta = auth
                    .lock()
                    .unwrap()
                    .validate(&token)
                    .ok_or_else(|| RpcError {
                        code: AUTH_FAILED,
                        message: "invalid, expired, or revoked bearer token".into(),
                        data: None,
                    })?;
                (
                    meta.principal.clone(),
                    meta.caps.clone(),
                    meta.profile.clone(),
                    false,
                    "bearer",
                    Some(meta),
                )
            } else if local_auth.unwrap_or_default() == LocalAuthMode::LocalHuman {
                (
                    principal(),
                    vec![],
                    "local-human".into(),
                    true,
                    "local-human",
                    None,
                )
            } else {
                (
                    "agent:mcp".into(),
                    vec![],
                    "restricted-agent".into(),
                    false,
                    "restricted-agent",
                    None,
                )
            };
        let can_approve = local_human
            || profile == "supervisor"
            || token_caps.iter().any(|cap| cap == "plan.approve");
        let name = params.session.unwrap_or_else(|| "default".into());
        let session = self.session(&name, &who)?;
        session.ensure_healthy()?;
        let cwd = session
            .evaluator
            .lock()
            .unwrap()
            .cwd()
            .as_os_str()
            .to_owned();
        // A connection may reattach to another principal-private session.
        // Subscriptions belong to the previous owner and must not silently
        // follow the socket into the new attachment.
        if attached.is_some() {
            self.events.remove_conn(client);
        }
        *attached = Some(Attachment {
            session,
            principal: who.clone(),
            can_approve,
            tty,
            cancel_epoch: None,
            bearer,
            security_epoch: ATTACH_SECURITY_EPOCH,
        });
        // site/content/internals/language-conformance-contract.md tier honesty: report the REAL strongest OS backend
        // available on this host (Landlock → A, Seatbelt → C, else
        // advisory D), and whether this principal's spawns will
        // *actually* be confined — true only when a genuine OS backend
        // exists AND this principal's policy resolves to a real sandbox
        // (a scoped agent), never for the default-permissive human.
        let status = EnforcementStatus::detect();
        let tier = tier_letter(status.available_tier);
        let caps_enforced = self.caps_enforced_for(&who);
        let mut result = serde_json::to_value(AttachResult {
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
        .map_err(internal)?;
        let object = result
            .as_object_mut()
            .expect("AttachResult always serializes to an object");
        object.insert("auth_mode".into(), Json::String(auth_mode.into()));
        object.insert(
            "session_isolation".into(),
            Json::String(PRINCIPAL_SESSION_ISOLATION.into()),
        );
        object.insert("security_epoch".into(), Json::from(ATTACH_SECURITY_EPOCH));
        Ok(result)
    }

    /// `session.env` (site/content/internals/kernel-protocol.md, `shoal://session/env`): the session's
    /// environment read from its own evaluator (the same source the in-language
    /// `env` builtin reads, so in-session env writes are reflected), the same
    /// way `session.attach` reads `cwd()`. Env is **NAMES-only unless granted**
    /// — the values travel only when this principal's policy resolves `EnvRead`
    /// to `Allow` (a default-permissive human does; a scoped agent that wasn't
    /// granted an env read sees the names alone, never a guess). The `granted`
    /// flag tells the reader which of the two it got.
    pub(crate) fn handle_session_env(
        self: &Arc<Self>,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let pairs: Vec<(String, String)> = {
            let evaluator = session.evaluator.lock().unwrap();
            evaluator
                .env_vars()
                .iter()
                .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v.to_str()?.to_string())))
                .collect()
        };
        let mut names: Vec<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
        names.sort();
        let granted = self.policy.evaluate_effect(
            &attachment.principal,
            &Effect::EnvRead {
                names: names.clone(),
            },
        ) == Verdict::Allow;
        if granted {
            let env: serde_json::Map<String, Json> = pairs
                .into_iter()
                .map(|(k, v)| (k, Json::String(v)))
                .collect();
            encode(json!({"granted": true, "names": names, "env": env}))
        } else {
            encode(json!({"granted": false, "names": names}))
        }
    }

    /// `session.reef` (site/content/internals/kernel-protocol.md, `shoal://session/reef`): the session's
    /// reef resolution state — the active manifest scope and every constrained
    /// tool's binding (locked version/provider, or an honest `null` gap when a
    /// scope constrains a tool that isn't locked yet). Sourced entirely from the
    /// evaluator's cached scope chain + loaded lock via
    /// [`Evaluator::prompt_reef_snapshot`] — zero subprocess, zero fresh
    /// resolution (site/content/internals/reef-resolution.md, site/content/internals/kernel-protocol.md).
    pub(crate) fn handle_session_reef(
        self: &Arc<Self>,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let snapshot = {
            let mut evaluator = session.evaluator.lock().unwrap();
            evaluator.prompt_reef_snapshot()
        };
        let bindings: Vec<Json> = snapshot
            .bindings
            .iter()
            .map(|b| {
                json!({
                    "tool": b.tool,
                    "version": b.version,
                    "provider": b.provider,
                    "scope": b.scope,
                    "constrained": b.constrained,
                })
            })
            .collect();
        encode(json!({
            "active_scope": snapshot.active_scope,
            "bindings": bindings,
        }))
    }
}
