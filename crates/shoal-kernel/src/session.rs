//! Session state (`Session`, the attached-connection `Attachment`) plus the
//! `session.attach` dispatch handler. Split out of `lib.rs` (site/content/internals/roadmap-and-priorities.md
//! `site/content/internals/change-map.md`; pure mechanical move, zero wire/behavior change.
use super::*;

pub(crate) const MAX_SESSION_NAME_BYTES: usize = 256;
pub(crate) const MAX_CLIENT_KIND_BYTES: usize = 128;

fn invalid_attach(message: &'static str) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message: message.into(),
        data: None,
    }
}

fn bounded_attach_text(value: &Json, max_bytes: usize) -> bool {
    value.as_str().is_some_and(|text| {
        !text.is_empty() && text.len() <= max_bytes && !text.chars().any(char::is_control)
    })
}

/// Validate the small attach schema before authentication performs hashing,
/// locks the authority store, or retains caller-controlled identity strings.
/// Errors are intentionally constant and never quote credential material.
fn validate_attach_wire(params: &Json) -> Result<Option<LocalAuthMode>, RpcError> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_attach("session.attach params must be an object"))?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "session" | "token" | "local_auth" | "client"))
    {
        return Err(invalid_attach("unknown session.attach parameter"));
    }
    if let Some(session) = object.get("session")
        && !session.is_null()
        && !bounded_attach_text(session, MAX_SESSION_NAME_BYTES)
    {
        return Err(invalid_attach("invalid session name"));
    }
    if let Some(token) = object.get("token")
        && !token.is_null()
        && token.as_str().is_none()
    {
        return Err(invalid_attach("bearer token must be a string"));
    }
    let client = object
        .get("client")
        .and_then(Json::as_object)
        .ok_or_else(|| invalid_attach("session.attach client must be an object"))?;
    if client
        .keys()
        .any(|key| !matches!(key.as_str(), "kind" | "tty"))
    {
        return Err(invalid_attach("unknown session.attach client parameter"));
    }
    if !client
        .get("kind")
        .is_some_and(|kind| bounded_attach_text(kind, MAX_CLIENT_KIND_BYTES))
        || !client.get("tty").is_some_and(Json::is_boolean)
    {
        return Err(invalid_attach("invalid session.attach client"));
    }
    match object.get("local_auth") {
        Some(value) => serde_json::from_value::<LocalAuthMode>(value.clone())
            .map(Some)
            .map_err(|_| invalid_attach("invalid local authentication mode")),
        None => Ok(None),
    }
}

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
    /// plan. An embedded process-trust-root local human is trusted; bearer
    /// attachments must opt in through the machine-admin `supervisor` profile
    /// or `plan.approve` capability. A bearer profile named `local-human` is
    /// intentionally not human-presence evidence.
    pub(crate) can_approve: bool,
    /// Whether the attaching client declared itself a real interactive
    /// terminal (`session.attach`'s `client.tty`). Every client this
    /// `shoal-mcp` attaches with `tty:false`; the default private `shoal` REPL
    /// attaches over an inherited human transport with `tty:true`. The latter
    /// selects the interactive bootstrap profile (including init files) and
    /// preserves terminal color; durable/headless clients cannot assert that
    /// profile over a public socket.
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
    /// Immutable server-selected provenance for this connection.
    pub(crate) connection_trust: ConnectionTrust,
}

impl Attachment {
    /// Token administration is narrower than plan approval/shutdown. A public
    /// machine credential must carry the explicit `token.admin` capability;
    /// profile strings and `plan.approve` do not imply credential issuance.
    pub(crate) fn can_admin_tokens(&self) -> bool {
        self.connection_trust == ConnectionTrust::EmbeddedHuman
            || self
                .bearer
                .as_ref()
                .is_some_and(|meta| meta.caps.iter().any(|cap| cap == "token.admin"))
    }
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
    /// Success-only mapping aligned with the evaluator-visible bounded `out`
    /// list. Entries are evaluator statement journal IDs, never the kernel's
    /// coarser outer exec rows.
    out_entries: Mutex<VecDeque<Option<i64>>>,
    stream_cursors: Mutex<HashMap<StreamCursorRef, Arc<WireStreamCursorEntry>>>,
    blob_decompressions: Mutex<VecDeque<Instant>>,
}

pub(crate) const MAX_TRANSCRIPT_PER_SESSION: usize = 4096;
pub(crate) const MAX_WIRE_STREAM_CURSORS: usize = 64;

pub(crate) struct WireStreamCursor {
    pub(crate) upstream: Option<Box<dyn shoal_value::Upstream>>,
    pub(crate) next_seq: u64,
    pub(crate) done: bool,
}

pub(crate) struct WireStreamCursorEntry {
    pub(crate) cancel: shoal_exec::CancelToken,
    /// Set before a deadline/close detaches the cursor. Workers check it at
    /// cooperative boundaries and must never publish a result after it flips.
    pub(crate) quarantined: AtomicBool,
    pub(crate) inner: Mutex<WireStreamCursor>,
}

impl WireStreamCursorEntry {
    pub(crate) fn quarantine(&self) {
        self.quarantined.store(true, Ordering::SeqCst);
        self.cancel.cancel();
    }

    pub(crate) fn lock_cursor(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, WireStreamCursor>, RpcError> {
        if self.quarantined.load(Ordering::SeqCst) {
            return Err(cursor_quarantined());
        }
        match self.inner.lock() {
            Ok(cursor) => Ok(cursor),
            Err(poisoned) => {
                // Cursor-local failure: never inspect the unknown upstream or
                // sequence state. Cancel and detach only this cursor.
                drop(poisoned);
                self.quarantine();
                Err(cursor_quarantined())
            }
        }
    }
}

impl Drop for WireStreamCursorEntry {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
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
        if self.quarantined.load(Ordering::SeqCst)
            || self.evaluator.is_poisoned()
            || self.transcript.is_poisoned()
            || self.out_entries.is_poisoned()
            || self.stream_cursors.is_poisoned()
            || self.blob_decompressions.is_poisoned()
        {
            self.quarantine();
            Err(self.quarantined_error())
        } else {
            Ok(())
        }
    }

    pub(crate) fn reserve_blob_decompression(
        &self,
        max: usize,
        window: std::time::Duration,
    ) -> Result<(), RpcError> {
        self.ensure_healthy()?;
        let now = Instant::now();
        let mut recent = self.blob_decompressions.lock().map_err(|poisoned| {
            drop(poisoned);
            self.quarantine();
            self.quarantined_error()
        })?;
        while recent
            .front()
            .is_some_and(|started| now.duration_since(*started) >= window)
        {
            recent.pop_front();
        }
        if recent.len() >= max {
            let retry_after = recent
                .front()
                .map(|started| {
                    started
                        .checked_add(window)
                        .unwrap_or(now)
                        .saturating_duration_since(now)
                        .as_millis() as u64
                })
                .unwrap_or(0);
            return Err(RpcError {
                code: QUOTA_EXCEEDED,
                message: "CAS decompression rate limit exceeded".into(),
                data: Some(json!({
                    "limit": "blob_decompressions_per_window",
                    "max": max,
                    "window_ms": window.as_millis() as u64,
                    "retry_after_ms": retry_after,
                    "owner": {"principal": &self.key.principal, "session": &self.key.name},
                })),
            });
        }
        recent.push_back(now);
        Ok(())
    }

    fn quarantined_error(&self) -> RpcError {
        RpcError {
            code: INTERNAL_ERROR,
            message: "session is quarantined after an internal state failure".into(),
            data: Some(json!({"session": self.id, "session_quarantined": true})),
        }
    }

    pub(crate) fn lock_evaluator(&self) -> Result<std::sync::MutexGuard<'_, Evaluator>, RpcError> {
        self.ensure_healthy()?;
        match self.evaluator.lock() {
            Ok(evaluator) => Ok(evaluator),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                Err(self.quarantined_error())
            }
        }
    }

    pub(crate) fn lock_transcript(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<Ref, Value>>, RpcError> {
        self.ensure_healthy()?;
        match self.transcript.lock() {
            Ok(transcript) => Ok(transcript),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                Err(self.quarantined_error())
            }
        }
    }

    fn lock_out_entries(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, VecDeque<Option<i64>>>, RpcError> {
        self.ensure_healthy()?;
        match self.out_entries.lock() {
            Ok(entries) => Ok(entries),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                Err(self.quarantined_error())
            }
        }
    }

    fn lock_stream_cursors(
        &self,
    ) -> Result<
        std::sync::MutexGuard<'_, HashMap<StreamCursorRef, Arc<WireStreamCursorEntry>>>,
        RpcError,
    > {
        self.ensure_healthy()?;
        match self.stream_cursors.lock() {
            Ok(cursors) => Ok(cursors),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                Err(self.quarantined_error())
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn insert_transcript(&self, value_ref: Ref, value: Value) {
        let _ = self.insert_transcript_checked(value_ref, value);
    }

    pub(crate) fn insert_transcript_checked(
        &self,
        value_ref: Ref,
        value: Value,
    ) -> Result<(), RpcError> {
        let mut transcript = self.lock_transcript()?;
        transcript.try_reserve(1).map_err(|error| RpcError {
            code: INTERNAL_ERROR,
            message: format!("cannot reserve session transcript entry: {error}"),
            data: Some(json!({"resource": "session_transcript"})),
        })?;
        Self::insert_transcript_retained(&mut transcript, value_ref, value);
        Ok(())
    }

    pub(crate) fn insert_transcript_retained(
        transcript: &mut HashMap<Ref, Value>,
        value_ref: Ref,
        value: Value,
    ) {
        let id = value_ref
            .0
            .split_once(':')
            .and_then(|(_, id)| id.parse::<u64>().ok());
        transcript.insert(value_ref, value);
        if let Some(id) = id
            && id > MAX_TRANSCRIPT_PER_SESSION as u64
        {
            transcript.remove(&Ref::new("out", id - MAX_TRANSCRIPT_PER_SESSION as u64));
        }
    }

    pub(crate) fn rewrite_out_undo(&self, program: &mut Program) {
        if let Ok(mut entries) = self.lock_out_entries() {
            resolve_out_undo(program, entries.make_contiguous());
        }
    }

    pub(crate) fn push_out_entry(&self, entry_id: Option<i64>) {
        let Ok(mut entries) = self.lock_out_entries() else {
            return;
        };
        if entries.len() >= shoal_eval::MAX_REPL_TRANSCRIPT_VALUES {
            entries.pop_front();
        }
        entries.push_back(entry_id);
    }

    /// Get or lazily claim a transcript stream's single-consumer upstream.
    /// Cursor creation is serialized under the registry lock, so concurrent
    /// first pulls cannot both consume the same `StreamVal`.
    pub(crate) fn stream_cursor(
        &self,
        cursor: &StreamCursorRef,
    ) -> Result<Arc<WireStreamCursorEntry>, RpcError> {
        let mut cursors = self.lock_stream_cursors()?;
        if let Some(entry) = cursors.get(cursor) {
            return Ok(entry.clone());
        }

        // Terminal cursors retain no upstream resources. Reap them at the
        // admission boundary so clients do not need to close after observing
        // `done:true` merely to make quota progress.
        if cursors.len() >= MAX_WIRE_STREAM_CURSORS {
            cursors.retain(|_, entry| match entry.inner.lock() {
                Ok(cursor) => !cursor.done,
                Err(poisoned) => {
                    drop(poisoned);
                    entry.quarantine();
                    false
                }
            });
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
        let entry = Arc::new(WireStreamCursorEntry {
            cancel: shoal_exec::CancelToken::new(),
            quarantined: AtomicBool::new(false),
            inner: Mutex::new(WireStreamCursor {
                upstream: Some(upstream),
                next_seq: 0,
                done: false,
            }),
        });
        cursors.insert(cursor.clone(), entry.clone());
        Ok(entry)
    }

    /// Explicitly release a cursor. If it has never been pulled, claim and
    /// immediately drop its upstream so source threads/resources are closed
    /// and later pulls correctly observe single consumption.
    pub(crate) fn close_stream_cursor(&self, cursor: &StreamCursorRef) -> Result<bool, RpcError> {
        if let Some(entry) = self.lock_stream_cursors()?.remove(cursor) {
            // Never wait for an in-process upstream while serving close. A
            // cooperative worker observes cancellation; a non-cooperative
            // trusted extension retains this detached Arc only until its
            // globally-leased worker eventually returns.
            entry.quarantine();
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

    pub(crate) fn quarantine_stream_cursor(
        &self,
        cursor: &StreamCursorRef,
        observed: &Arc<WireStreamCursorEntry>,
    ) {
        observed.quarantine();
        if let Ok(mut cursor) = observed.inner.try_lock() {
            cursor.done = true;
            cursor.upstream.take();
        }
        let removed = {
            let Ok(mut cursors) = self.lock_stream_cursors() else {
                return;
            };
            cursors
                .get(cursor)
                .is_some_and(|current| Arc::ptr_eq(current, observed))
                .then(|| cursors.remove(cursor))
                .flatten()
        };
        drop(removed);
    }

    fn resolve_stream_value(
        &self,
        cursor: &StreamCursorRef,
    ) -> Result<shoal_value::StreamVal, RpcError> {
        let transcript = self.lock_transcript()?;
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

    #[cfg(test)]
    pub(crate) fn has_stream_cursor(&self, cursor: &StreamCursorRef) -> bool {
        self.lock_stream_cursors()
            .is_ok_and(|cursors| cursors.contains_key(cursor))
    }
}

fn unknown_stream() -> RpcError {
    RpcError {
        code: UNKNOWN_REF,
        message: "unknown stream cursor".into(),
        data: None,
    }
}

pub(crate) fn cursor_quarantined() -> RpcError {
    RpcError {
        code: INTERNAL_ERROR,
        message: "stream cursor is quarantined after an internal state failure".into(),
        data: Some(json!({"stream_cursor_quarantined": true})),
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

fn resolve_out_undo(program: &mut Program, out_entries: &[Option<i64>]) {
    for stmt in &mut program.stmts {
        let Stmt::Expr {
            expr: Expr::Cmd { call, .. },
            ..
        } = stmt
        else {
            continue;
        };
        if call.head != "undo" || call.args.len() != 1 {
            continue;
        }
        let Some(index) = out_index_literal(&call.args[0]) else {
            continue;
        };
        let resolved = if index >= 0 {
            usize::try_from(index).ok()
        } else {
            index
                .checked_abs()
                .and_then(|distance| usize::try_from(distance).ok())
                .and_then(|distance| out_entries.len().checked_sub(distance))
        };
        let Some(Some(entry_id)) = resolved.and_then(|index| out_entries.get(index)) else {
            continue;
        };
        let span = call.args[0].span();
        call.args[0] = CmdArg::Expr {
            expr: Expr::Int {
                value: *entry_id,
                span,
            },
            span,
        };
    }
}

fn out_index_literal(arg: &CmdArg) -> Option<i64> {
    let CmdArg::Expr {
        expr: Expr::Index { recv, index, .. },
        ..
    } = arg
    else {
        return None;
    };
    let Expr::Var { name, .. } = recv.as_ref() else {
        return None;
    };
    if name != "out" {
        return None;
    }
    match index.as_ref() {
        Expr::Int { value, .. } => Some(*value),
        Expr::Unary {
            op: UnOp::Neg,
            expr,
            ..
        } => match expr.as_ref() {
            Expr::Int { value, .. } => value.checked_neg(),
            _ => None,
        },
        _ => None,
    }
}

impl Kernel {
    /// Get-or-create the principal-private named session.
    #[cfg(test)]
    pub(crate) fn session(&self, name: &str, principal: &str) -> Result<Arc<Session>, RpcError> {
        self.session_with_surface(name, principal, shoal_host::Surface::Kernel)
    }

    /// Get-or-create a session under an explicit host profile. Only the
    /// inherited private human transport may select `Interactive`; ordinary
    /// kernel callers use [`Self::session`] and cannot execute init files.
    fn session_with_surface(
        &self,
        name: &str,
        principal: &str,
        surface: shoal_host::Surface,
    ) -> Result<Arc<Session>, RpcError> {
        let key = SessionKey::new(principal, name);
        self.sessions.get_or_try_insert_with(
            key.clone(),
            || {
                let cwd = std::env::current_dir().map_err(internal)?;
                let bootstrap = shoal_host::SessionBootstrap::discover(&cwd).map_err(internal)?;
                let mut evaluator = Evaluator::new(cwd);
                let report = bootstrap
                    .apply(&mut evaluator, surface, &key.principal)
                    .map_err(internal)?;
                for warning in bootstrap.config_warnings() {
                    eprintln!("shoal-kernel: warning: config: {warning}");
                }
                for warning in report.warnings {
                    eprintln!("shoal-kernel: warning: {warning}");
                }
                // Request execution sets this again at its own boundary to
                // prevent stale identity. Configured aliases/environment are
                // already seeded by `apply`; only the inherited private-human
                // interactive profile may run `init.files` below.
                evaluator.set_leash_policy(self.policy.clone(), key.principal.clone());
                // Long-lived agent/interactive sessions build up `j`/`jump` directory
                // history against the shared per-user store, same as the REPL (frecency
                // recording is best-effort and never fails a cd).
                evaluator.open_default_jump_history();
                // Install a command journal on the session's own evaluator (site/content/internals/language-conformance-contract.md),
                // mirroring the local REPL's `set_journal` call: without
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
                if bootstrap.config().journal.enabled
                    && let Some(state_dir) = &self.state_dir
                {
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
                    let _ = wire_bus.publish_user(&wire_owner, channel, json);
                }));
                bootstrap
                    .run_init(&mut evaluator, surface)
                    .map_err(internal)?;
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
                    out_entries: Mutex::new(VecDeque::new()),
                    stream_cursors: Mutex::new(HashMap::new()),
                    blob_decompressions: Mutex::new(VecDeque::new()),
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

    pub(crate) fn handle_session_snapshot(
        &self,
        attached: &Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let mut evaluator = attachment.session.lock_evaluator()?;
        let mut names = evaluator.env().visible_names();
        names.sort();
        let bindings = names
            .into_iter()
            .filter_map(|name| {
                evaluator.env().get(&name).map(|value| {
                    json!({
                        "name": name,
                        "callable": value.is_callable(),
                        "type": value.type_name(),
                    })
                })
            })
            .collect::<Vec<_>>();
        let jobs = evaluator.jobs_snapshot();
        let last_value = wire_value(evaluator.it());
        let cwd = WirePath::encode(evaluator.cwd().as_os_str());
        // Project only the information completion actually needs. Shipping
        // the full process environment would expose unrelated secrets; raw
        // PATH would also leave relative entries ambiguous on the client.
        // Resolve, deduplicate, and bound the directory list server-side.
        let mut completion_dirs = Vec::<PathBuf>::new();
        if let Some((_, path)) = evaluator
            .env_vars()
            .iter()
            .rev()
            .find(|(name, _)| name.as_bytes() == b"PATH")
        {
            for dir in std::env::split_paths(path).take(256) {
                let dir = if dir.is_absolute() {
                    dir
                } else {
                    evaluator.cwd().join(dir)
                };
                if !completion_dirs.contains(&dir) {
                    completion_dirs.push(dir);
                }
            }
        }
        let completion_dirs = completion_dirs
            .iter()
            .map(|dir| WirePath::encode(dir.as_os_str()))
            .collect::<Vec<_>>();
        let reef = evaluator.prompt_reef_snapshot();
        let status = EnforcementStatus::detect();
        let authority_tier = tier_letter(status.available_tier);
        let authority_enforced = self.caps_enforced_for(&attachment.principal);
        let authority_kind = if attachment.connection_trust == ConnectionTrust::EmbeddedHuman {
            "human"
        } else {
            "agent"
        };
        Ok(json!({
            "authority": {
                "principal": attachment.principal,
                "kind": authority_kind,
                "leash": {
                    "tier": authority_tier,
                    "enforced": authority_enforced,
                },
            },
            "cwd": cwd,
            "completion": {
                "path_dirs": completion_dirs,
            },
            "bindings": bindings,
            "jobs": {
                "running": jobs.running,
                "suspended": jobs.suspended,
                "total": jobs.total,
                "completed": jobs.completed,
            },
            "reef": {
                "active_scope": reef.active_scope,
                "bindings": reef.bindings.into_iter().map(|binding| json!({
                    "tool": binding.tool,
                    "version": binding.version,
                    "provider": binding.provider,
                    "scope": binding.scope,
                    "constrained": binding.constrained,
                })).collect::<Vec<_>>(),
            },
            "last_value": last_value,
        }))
    }

    pub(crate) fn handle_session_attach(
        self: &Arc<Self>,
        params: Json,
        client: u64,
        attached: &mut Option<Attachment>,
        connection_trust: ConnectionTrust,
    ) -> Result<Json, RpcError> {
        let local_auth = validate_attach_wire(&params)?;
        let params: AttachParams = decode(params)?;
        let tty = params.client.tty;
        if params.token.is_some() && local_auth.is_some() {
            return Err(RpcError {
                code: INVALID_PARAMS,
                message: "token and local_auth are mutually exclusive authentication modes".into(),
                data: None,
            });
        }
        if let Some(token) = params.token.as_deref()
            && !shoal_auth::bearer_is_canonical(token)
        {
            return Err(RpcError {
                code: AUTH_FAILED,
                message: "invalid, expired, or revoked bearer token".into(),
                data: None,
            });
        }
        if params.token.is_none()
            && local_auth == Some(LocalAuthMode::LocalHuman)
            && connection_trust != ConnectionTrust::EmbeddedHuman
        {
            return Err(RpcError {
                code: AUTH_FAILED,
                message: "durable kernels do not accept client-asserted human presence; use an explicit supervisor or plan.approve bearer for machine administration".into(),
                data: Some(json!({
                    "auth_mode": "local-human",
                    "human_presence_supported": false,
                    "connection_trust": connection_trust.as_str(),
                    "machine_admin_profiles": ["supervisor", "plan.approve"],
                })),
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
                    .map_err(|_| poisoned_subsystem("authentication token store"))?
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
        // An ordinary bearer is a machine credential, not proof that a human
        // is at the keyboard. In particular, a profile string controlled by
        // token creation must not manufacture human-presence authority.
        let can_approve = local_human
            || profile == "supervisor"
            || token_caps.iter().any(|cap| cap == "plan.approve");
        let name = params.session.unwrap_or_else(|| "default".into());
        let surface = if local_human && tty && connection_trust == ConnectionTrust::EmbeddedHuman {
            shoal_host::Surface::Interactive
        } else {
            shoal_host::Surface::Kernel
        };
        let session = self.session_with_surface(&name, &who, surface)?;
        session.ensure_healthy()?;
        self.ensure_event_owner(&session.key.owner())?;
        let cwd = session.lock_evaluator()?.cwd().as_os_str().to_owned();
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
            connection_trust,
        });
        // site/content/internals/language-conformance-contract.md tier honesty: report the REAL strongest OS backend
        // available on this host (Landlock → A, Seatbelt → C, else
        // advisory D), and whether this principal's spawns will
        // *actually* be confined — true only when a genuine OS backend
        // exists AND this principal's policy resolves to a real sandbox
        // (a scoped agent), never for the default-permissive human.
        let status = EnforcementStatus::detect();
        let tier = tier_letter(status.available_tier);
        let enforcement = self.enforcement_preview(&who);
        let caps_enforced = enforcement.filesystem_enforceable;
        let mut result = serde_json::to_value(AttachResult {
            session: name,
            principal: who.clone(),
            caps: json!({"enforced":caps_enforced,"tier":tier,"available_tier":tier,"policy_principal":who,"profile":profile,"token_caps":token_caps,"opaque":verdict_name(self.policy.evaluate_effect(&who, &Effect::Opaque))}),
            cwd: WirePath::encode(&cwd),
            env_hash: "local".into(),
            ast_version: AST_VERSION,
            caps_enforced,
            enforcement,
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
        object.insert(
            "connection_trust".into(),
            Json::String(connection_trust.as_str().into()),
        );
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
            let evaluator = session.lock_evaluator()?;
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
            let mut evaluator = session.lock_evaluator()?;
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

pub(crate) fn poisoned_subsystem(subsystem: &str) -> RpcError {
    RpcError {
        code: INTERNAL_ERROR,
        message: format!("{subsystem} is unavailable after an internal state failure"),
        data: Some(json!({"subsystem": subsystem, "quarantined": true})),
    }
}

#[cfg(test)]
mod poison_tests {
    use super::*;

    fn attachment(session: Arc<Session>) -> Option<Attachment> {
        Some(Attachment {
            session,
            principal: "principal:test".into(),
            can_approve: false,
            tty: false,
            cancel_epoch: None,
            bearer: None,
            security_epoch: ATTACH_SECURITY_EPOCH,
            connection_trust: ConnectionTrust::EmbeddedHuman,
        })
    }

    #[test]
    fn poisoned_evaluator_quarantines_only_its_session_with_stable_errors() {
        let kernel = Kernel::new();
        let poisoned = kernel.session("poisoned", "principal:test").unwrap();
        let healthy = kernel.session("healthy", "principal:test").unwrap();
        let poisoner = poisoned.clone();
        let thread = std::thread::spawn(move || {
            let _evaluator = poisoner
                .evaluator
                .lock()
                .expect("test lock should not be poisoned");
            panic!("inject evaluator poison");
        });
        assert!(thread.join().is_err());

        let attached = attachment(poisoned);
        for _ in 0..2 {
            let error = kernel
                .handle_session_snapshot(&attached)
                .expect_err("poisoned session must fail closed");
            assert_eq!(error.code, INTERNAL_ERROR);
            assert_eq!(error.data.unwrap()["session_quarantined"], true);
        }

        kernel
            .handle_session_snapshot(&attachment(healthy))
            .expect("a different session remains healthy");
    }

    #[test]
    fn snapshot_exposes_a_bounded_path_projection_not_arbitrary_environment() {
        let kernel = Kernel::new();
        let session = kernel.session("completion", "principal:test").unwrap();
        let snapshot = kernel
            .handle_session_snapshot(&attachment(session))
            .expect("healthy snapshot");

        assert!(snapshot.get("env").is_none());
        assert!(snapshot.get("environment").is_none());
        assert_eq!(snapshot["authority"]["principal"], "principal:test");
        assert_eq!(snapshot["authority"]["kind"], "human");
        assert!(snapshot["authority"]["leash"]["tier"].is_string());
        assert!(snapshot["authority"]["leash"]["enforced"].is_boolean());
        let paths = snapshot["completion"]["path_dirs"]
            .as_array()
            .expect("completion path projection");
        assert!(paths.len() <= 256);
        for path in paths {
            let path: WirePath = serde_json::from_value(path.clone()).unwrap();
            let decoded = PathBuf::from(path.decode().unwrap());
            assert!(
                decoded.is_absolute(),
                "projected path was relative: {decoded:?}"
            );
        }
    }
}
