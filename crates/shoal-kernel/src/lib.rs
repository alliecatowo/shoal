//! Long-lived Unix-socket host for the shoal evaluator (site/content/internals/language-conformance-contract.md).

mod dispatch;
mod eventbus;
mod handlers_exec;
mod handlers_pty;
mod handlers_session;
mod handlers_task;
mod handlers_value;
mod session;
mod wire;

use eventbus::*;
use session::*;
use wire::*;

use serde_json::{Value as Json, json};
use shoal_ast::{Program, Stmt};
use shoal_auth::TokenStore;
use shoal_eval::{Evaluator, Position};
use shoal_journal::{EntryRecord, Journal, JournalQuery};
use shoal_leash::{
    Effect, EnforcementStatus, EnforcementTier, Estimates, Plan, Policy, Reversibility, Verdict,
};
use shoal_proto::error_code::*;
use shoal_proto::*;
use shoal_value::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub struct Kernel {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    next_client: AtomicU64,
    journal: Mutex<Journal>,
    /// The per-user state dir this kernel's own `journal` (above) was opened
    /// against, if any (`None` for the ephemeral in-memory kernels used by
    /// `new`/`with_policy`, which have no on-disk store at all). Kept around
    /// so `session()` can open a SECOND, independent `Journal` handle onto
    /// the exact same on-disk SQLite/WAL store for each session's own
    /// `Evaluator` (mirrors `crates/shoal/src/repl.rs`'s dual-handle
    /// pattern) — never a divergent path from the kernel's own journal.
    state_dir: Option<PathBuf>,
    policy: Policy,
    plans: Mutex<HashMap<String, StoredPlan>>,
    tasks: Mutex<HashMap<Ref, Arc<TaskEntry>>>,
    next_task: AtomicU64,
    /// Long-lived interactive PTY sessions (site/content/internals/kernel-protocol.md), keyed by their
    /// `pty:{id}` ref like `tasks`. Each holds a live child on a real PTY plus
    /// its `vt100` emulator; scoped to the session that opened it. Dropped (and
    /// so terminated + reaped) on `pty.close` or when the kernel is dropped.
    ptys: Mutex<HashMap<Ref, Arc<PtyEntry>>>,
    next_pty: AtomicU64,
    auth: Option<Mutex<TokenStore>>,
    events: Arc<EventBus>,
    /// Whether a plan's requester may acknowledge its own plan via
    /// `cap.request` (HR-D3 self-acknowledgement). Default `false`: the approver
    /// principal MUST differ from the requester, so approval is a genuine
    /// separation-of-duties boundary (a supervising human/agent), not a
    /// rubber stamp the requesting agent applies to itself. Enabled explicitly
    /// (env `SHOAL_ALLOW_SELF_ACK`, or [`Kernel::set_allow_self_ack`]) for
    /// single-operator setups that knowingly accept self-approval. See
    /// `site/content/internals/security-threat-model.md`.
    allow_self_ack: AtomicBool,
}

/// Wire version of the AST node-kind vocabulary (site/content/internals/language-conformance-contract.md, site/content/internals/values-streams-execution.md). Bumped
/// from 1 to 2 when `sh_raw` was retired in favor of the general
/// `lang_block` node — a breaking rename to the AST-kind enum.
const AST_VERSION: u32 = 2;

struct TaskEntry {
    task: Ref,
    session: Arc<Session>,
    started_ns: i64,
    inner: Mutex<TaskInner>,
    done: Condvar,
    cancel: shoal_exec::CancelToken,
    cancel_requested: AtomicBool,
}
struct TaskInner {
    state: &'static str,
    finished_ns: Option<i64>,
    result_ref: Option<Ref>,
    error: Option<RpcError>,
}

/// A registered interactive PTY session (site/content/internals/kernel-protocol.md). The live
/// [`shoal_exec::PtySession`] (child + PTY master + `vt100` emulator) sits
/// behind a `Mutex` so `pty.send`/`pty.read`/`pty.resize`/`pty.close` from
/// different connections serialize on it. `session_id`/`principal` scope it to
/// its opener the same way [`TaskEntry`] scopes a task.
struct PtyEntry {
    session_id: String,
    #[allow(dead_code)] // recorded for parity with tasks / future auditing
    principal: String,
    cmd: String,
    session: Mutex<shoal_exec::PtySession>,
}

struct StoredPlan {
    src: String,
    session: String,
    /// The plan owner / **requester** — the principal that derived this plan
    /// (`exec {mode:"plan"}`). Distinct from an `ApprovalRecord::approver`.
    principal: String,
    plan: Plan,
    approved: bool,
    /// The auditable approval binding, present once a `cap.request` approved
    /// this plan (HR-D2). Binds requester, plan ref/hash, approver, granted
    /// scope, when it was approved, and — once the approved plan actually runs
    /// — the journal entry id of the execution that consumed it.
    approval: Option<ApprovalRecord>,
}

/// The auditable record binding an approval to its requester, plan, approver,
/// scope, and consuming execution (HR-D2). Mirrored into the journal as an
/// audit entry at approval time (`record_approval_audit`) and surfaced on
/// `plan.get` so the whole chain is inspectable, never an unattributed bit.
#[derive(Clone)]
struct ApprovalRecord {
    /// The plan owner whose effects were approved.
    requester: String,
    /// The distinct principal that approved (the `cap.request` caller).
    approver: String,
    /// The source-anchored plan ref/hash this approval is bound to.
    plan_ref: String,
    /// The effect kinds the approval was scoped to (empty ⇒ the whole plan).
    scope: Vec<String>,
    /// When the approval was granted (ns since epoch).
    approved_at_ns: i64,
    /// The journal entry id of the execution that consumed this approval, once
    /// an approved `exec` actually ran the plan. `None` until then.
    consumed_by: Option<i64>,
}

impl Kernel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            journal: Mutex::new(Journal::in_memory().expect("in-memory journal")),
            state_dir: None,
            policy: permissive_policy(),
            plans: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            next_task: AtomicU64::new(1),
            ptys: Mutex::new(HashMap::new()),
            next_pty: AtomicU64::new(1),
            events: Arc::new(EventBus::default()),
            auth: None,
            allow_self_ack: AtomicBool::new(self_ack_from_env()),
        })
    }

    pub fn open(state_dir: impl AsRef<Path>) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let state_dir = state_dir.as_ref();
        let journal = Journal::open(state_dir)?;
        let events = EventBus::default();
        // Reopening an EXISTING on-disk store must resume its
        // `journal`/`session.transcript` seq state, not restart both at 0 —
        // see `EventBus::seed_from_journal` for why (a reconnecting agent's
        // persisted `since=N` cursor would otherwise collide with a
        // brand-new seq the freshly-restarted kernel hands out starting
        // from 0 again). A fresh, empty store is a no-op: both channels
        // correctly still start at 0.
        events.seed_from_journal(&journal);
        Ok(Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            journal: Mutex::new(journal),
            state_dir: Some(state_dir.to_path_buf()),
            policy: permissive_policy(),
            plans: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            next_task: AtomicU64::new(1),
            ptys: Mutex::new(HashMap::new()),
            next_pty: AtomicU64::new(1),
            events: Arc::new(events),
            auth: Some(Mutex::new(TokenStore::open(state_dir.join("tokens.json"))?)),
            allow_self_ack: AtomicBool::new(self_ack_from_env()),
        }))
    }

    pub fn open_with_policy(
        state_dir: impl AsRef<Path>,
        policy: Policy,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let state_dir = state_dir.as_ref();
        let journal = Journal::open(state_dir)?;
        let events = EventBus::default();
        // Same restart-seq-continuity fix as `Kernel::open` above.
        events.seed_from_journal(&journal);
        Ok(Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            journal: Mutex::new(journal),
            state_dir: Some(state_dir.to_path_buf()),
            policy,
            plans: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            next_task: AtomicU64::new(1),
            ptys: Mutex::new(HashMap::new()),
            next_pty: AtomicU64::new(1),
            events: Arc::new(events),
            auth: Some(Mutex::new(TokenStore::open(state_dir.join("tokens.json"))?)),
            allow_self_ack: AtomicBool::new(self_ack_from_env()),
        }))
    }

    pub fn with_policy(policy: Policy) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            journal: Mutex::new(Journal::in_memory().expect("in-memory journal")),
            state_dir: None,
            policy,
            plans: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            next_task: AtomicU64::new(1),
            ptys: Mutex::new(HashMap::new()),
            next_pty: AtomicU64::new(1),
            events: Arc::new(EventBus::default()),
            auth: None,
            allow_self_ack: AtomicBool::new(self_ack_from_env()),
        })
    }

    pub fn serve(self: Arc<Self>, path: impl AsRef<Path>) -> io::Result<()> {
        self.serve_until(path, Arc::new(AtomicBool::new(false)))
    }

    pub fn serve_until(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(path)?;
        let _socket_guard = BoundSocket(path.to_path_buf());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        while !stop.load(Ordering::SeqCst) {
            let kernel = self.clone();
            match listener.accept() {
                Ok((stream, _)) => {
                    // The listener is non-blocking so the accept loop can poll
                    // `stop`, but that non-blocking flag is inherited by the
                    // accepted stream on some platforms (e.g. macOS) and not
                    // others (e.g. Linux, where accepted sockets are always
                    // blocking regardless of the listener's flag). Explicitly
                    // force the accepted connection back into blocking mode so
                    // per-connection reads in `handle_stream` block as intended
                    // on every platform, instead of racing the client's next
                    // write and getting a transient `WouldBlock` misread as EOF.
                    stream.set_nonblocking(false)?;
                    std::thread::spawn(move || {
                        let _ = kernel.handle_stream(stream);
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(25))
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn handle_stream(self: &Arc<Self>, stream: UnixStream) -> io::Result<()> {
        let client = self.next_client.fetch_add(1, Ordering::Relaxed);
        let mut reader = BufReader::new(stream.try_clone()?);
        let writer: SharedWriter = Arc::new(Mutex::new(stream));
        let mut attached: Option<Attachment> = None;
        let result = (|| -> io::Result<()> {
            while let Some(request) = read_frame(&mut reader)? {
                let id = request.id.clone();
                let response = if request.jsonrpc != JSONRPC {
                    Response::err(id, INVALID_REQUEST, "invalid JSON-RPC version", None)
                } else {
                    self.dispatch(request, client, &mut attached, Some(&writer))
                };
                write_frame(&mut *writer.lock().unwrap(), &response)?;
            }
            Ok(())
        })();
        // On disconnect, drop this connection's subscriptions so publish never
        // writes to a dead fd.
        self.events.remove_conn(client);
        result
    }

    fn task(&self, task: &Ref) -> Result<Arc<TaskEntry>, RpcError> {
        self.tasks
            .lock()
            .unwrap()
            .get(task)
            .cloned()
            .ok_or_else(|| RpcError {
                code: UNKNOWN_TASK,
                message: "unknown task ref".into(),
                data: None,
            })
    }

    /// Look up a live PTY session by ref, enforcing that it belongs to the
    /// calling session (an unknown ref and another session's ref are the same
    /// opaque not-found, mirroring `task`).
    fn pty(&self, pty_id: &Ref, session_id: &str) -> Result<Arc<PtyEntry>, RpcError> {
        let entry = self
            .ptys
            .lock()
            .unwrap()
            .get(pty_id)
            .cloned()
            .ok_or_else(unknown_pty)?;
        if entry.session_id != session_id {
            return Err(unknown_pty());
        }
        Ok(entry)
    }

    /// The real enforcement truth for `principal` (site/content/internals/language-conformance-contract.md tier honesty):
    /// `true` only when a genuine OS backend (Landlock/Seatbelt) exists on
    /// this host *and* the policy actually resolves a real sandbox for this
    /// principal — never for the default-permissive human. Single source of
    /// truth shared by `session.attach`'s `caps_enforced` and
    /// `cap.request`'s grant response (see `site/content/internals/security-threat-model.md`): an
    /// agent that unstuck an `approval_pending` plan via `cap.request` must
    /// get the SAME honest answer `attach` already gives, not a hardcoded
    /// `false` that systematically under-reports enforcement it actually has.
    pub(crate) fn caps_enforced_for(&self, principal: &str) -> bool {
        let status = EnforcementStatus::detect();
        let backend_present = matches!(
            status.available_tier,
            EnforcementTier::A | EnforcementTier::C
        );
        backend_present && self.policy.sandbox_for(principal).is_some()
    }

    /// Permit (or forbid) a plan's requester to acknowledge its own plan via
    /// `cap.request` (HR-D3). Default is forbidden — approval must come from a
    /// distinct principal. Enable only for single-operator setups that knowingly
    /// accept self-approval; the kernel binary honors `SHOAL_ALLOW_SELF_ACK` for
    /// the same purpose.
    pub fn set_allow_self_ack(&self, allow: bool) {
        self.allow_self_ack.store(allow, Ordering::SeqCst);
    }

    /// Append a journal audit entry for an approval decision (HR-D2), so the
    /// requester→plan→approver→scope binding is durably queryable via
    /// `journal.query`, not just live in the plan map. Best effort: an
    /// audit-write failure must never fail the approval it records (the same
    /// degrade-don't-brick stance the exec journal already takes). `session` is
    /// the plan's session (so the record is queryable in context) and
    /// `effect_kinds` is the plan's full effect set.
    fn record_approval_audit(
        &self,
        approval: &ApprovalRecord,
        effect_kinds: &[String],
        session: &str,
    ) {
        let effects_json = serde_json::to_string(&json!([{
            "kind": "approval",
            "plan_ref": approval.plan_ref,
            "requester": approval.requester,
            "approver": approval.approver,
            "scope": approval.scope,
            "effects": effect_kinds,
        }]))
        .unwrap_or_else(|_| "[]".into());
        let record = EntryRecord {
            session: session.to_string(),
            principal: approval.approver.clone(),
            ts_ns: approval.approved_at_ns,
            cwd: Vec::new(),
            src: format!(
                "# approval {} by {} for {}",
                approval.plan_ref, approval.approver, approval.requester
            ),
            ast_json: "null".into(),
            effects_json,
            opaque: false,
        };
        let journal = self.journal.lock().unwrap();
        if let Ok(id) = journal.append(&record) {
            let _ = journal.finish(id, Some(0), true, 0);
        }
    }
}

fn task_record(task: &Arc<TaskEntry>) -> TaskRecord {
    let inner = task.inner.lock().unwrap();
    task_record_locked(task, &inner)
}
fn task_record_locked(task: &TaskEntry, inner: &TaskInner) -> TaskRecord {
    TaskRecord {
        task: task.task.clone(),
        session: task.session.id.clone(),
        state: inner.state.into(),
        started_ns: task.started_ns,
        finished_ns: inner.finished_ns,
        result_ref: inner.result_ref.clone(),
        error: inner.error.clone(),
    }
}

struct BoundSocket(std::path::PathBuf);
impl Drop for BoundSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Json) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|e| RpcError {
        code: INVALID_PARAMS,
        message: e.to_string(),
        data: None,
    })
}
fn encode<T: serde::Serialize>(value: T) -> Result<Json, RpcError> {
    serde_json::to_value(value).map_err(internal)
}
fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError {
        code: INTERNAL_ERROR,
        message: error.to_string(),
        data: None,
    }
}
fn not_attached() -> RpcError {
    RpcError {
        code: NOT_ATTACHED,
        message: "attach to a session first".into(),
        data: None,
    }
}
fn unknown_pty() -> RpcError {
    RpcError {
        code: UNKNOWN_PTY,
        message: "unknown or closed pty_id".into(),
        data: None,
    }
}
fn principal() -> String {
    format!("uid:{}", unsafe { libc_geteuid() })
}
fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}
fn elapsed_ns(start: Instant) -> i64 {
    start.elapsed().as_nanos().min(i64::MAX as u128) as i64
}
fn permissive_policy() -> Policy {
    Policy::permissive(&principal())
}

/// Whether self-acknowledgement (a plan's requester approving its own plan via
/// `cap.request`) is permitted by process configuration (HR-D3). Off unless the
/// operator sets a non-empty `SHOAL_ALLOW_SELF_ACK`. Read once per kernel at
/// construction; `Kernel::set_allow_self_ack` overrides it at runtime.
fn self_ack_from_env() -> bool {
    std::env::var_os("SHOAL_ALLOW_SELF_ACK").is_some_and(|v| !v.is_empty())
}

/// The single-letter wire form of an enforcement tier (site/content/internals/language-conformance-contract.md): A (Landlock),
/// B (namespace fallback), C (Seatbelt), D (advisory). Reported at attach so a
/// client learns the strongest OS backend available on this host.
fn tier_letter(tier: EnforcementTier) -> &'static str {
    match tier {
        EnforcementTier::A => "A",
        EnforcementTier::B => "B",
        EnforcementTier::C => "C",
        EnforcementTier::D => "D",
    }
}

/// Derive a plan's real effects (site/content/internals/language-conformance-contract.md) and give it a source-anchored
/// `plan_ref`. Two distinct programs never collide, even when both derive to
/// the same coarse effect set (e.g. two different `sh { }` blocks, both
/// opaque) — the ref is a blake3 hash over the AST JSON *and* the effects,
/// not effects alone. Falls back to a conservative opaque plan if effect
/// derivation itself errors (arg-shape errors etc.); that must never block
/// real execution, which is the authority on whether the command runs.
fn derive_plan(evaluator: &mut Evaluator, ast: &Program, ast_json: &str) -> Plan {
    let mut plan = evaluator.plan_program(ast).unwrap_or_else(|_| {
        Plan::new(
            vec![Effect::Opaque],
            Reversibility::Unknown,
            Estimates::default(),
        )
    });
    plan.plan_ref = canonical_plan_ref(ast_json, &plan.effects);
    plan
}

fn canonical_plan_ref(ast_json: &str, effects: &[Effect]) -> String {
    let effects_json = serde_json::to_string(effects).unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(ast_json.as_bytes());
    hasher.update(b"\0");
    hasher.update(effects_json.as_bytes());
    format!("plan:{}", &hasher.finalize().to_hex()[..16])
}

/// `position: "value"` (site/content/internals/language-conformance-contract.md): evaluate the sole top-level command
/// expression without statement-position's raise-on-non-ok, binding `it` to
/// whatever comes back (including a failed outcome). Anything shaped other
/// than a single bare expression statement has no meaningful non-statement
/// reading (`let`/`fn`/`for`/… are already position-agnostic), so it falls
/// back to ordinary statement evaluation.
fn eval_with_position(
    evaluator: &mut Evaluator,
    ast: &Program,
    position: &str,
) -> shoal_value::VResult<Value> {
    if position == "value"
        && let Some((last, init)) = ast.stmts.split_last()
    {
        // Run every statement but the last with ordinary statement semantics,
        // sharing the evaluator's env so bindings carry into the final expr.
        if !init.is_empty() {
            evaluator.eval_program(&Program {
                stmts: init.to_vec(),
            })?;
        }
        // site/content/internals/language-conformance-contract.md: the *final* expression is the value; evaluate it in value
        // position so a failed outcome is captured (bound to `it`), not raised.
        if let Stmt::Expr { expr, .. } = last {
            let value = evaluator.eval_expr(expr, Position::Value)?;
            evaluator.it = value.clone();
            return Ok(value);
        }
        // A final `let`/`fn`/`for`/… has no distinct value reading; run it as
        // a statement and return whatever it produces.
        return evaluator.eval_program(&Program {
            stmts: vec![last.clone()],
        });
    }
    evaluator.eval_program(ast)
}
fn verdict_name(v: Verdict) -> &'static str {
    match v {
        Verdict::Allow => "allow",
        Verdict::Deny => "deny",
        Verdict::ApprovalRequired => "approval_required",
    }
}

/// Derive plan reversibility from its concrete effects (see
/// `site/content/internals/kernel-protocol.md`): irreversible for opaque work or network effects; reversible when
/// every effect is reversible/journaled (pure reads/writes, env, session,
/// time — AND filesystem deletes, see below). This is computed here rather
/// than trusting the leash's coarser `Reversibility` so the wire answer is
/// derived from the effect set the agent actually sees.
///
/// **`Effect::FsDelete` and the trash-vs-opaque distinction (bug fix,
/// judgment call documented here per the fix's instructions):** the only two
/// builtins that ever emit `FsDelete` are `rm` and `mv` (`shoal-eval`'s
/// `plan_effects.rs`); `sh{}`/any external command emits `Effect::Opaque`
/// instead and NEVER `FsDelete` — the two are structurally disjoint by
/// construction of the planner, so an `FsDelete` effect can never actually
/// originate from an opaque `sh { rm -rf }` (that stays caught by the
/// `Opaque` arm below, unconditionally). Given that, `FsDelete` is treated
/// as reversible: shoal's default `rm` moves files into a journaled trash
/// (`apply` fully recovers them; see `shoal-eval`'s `fs_undo_post`/
/// `record_trash_inverses` and `shoal-journal`'s `UndoInverse::TrashMove`),
/// and `mv`'s source-clearing "delete" is likewise undoable
/// (`UndoInverse::MoveBack`/`RestoreBytes`).
///
/// KNOWN LIMITATION: `Effect::FsDelete{paths}` carries no field
/// distinguishing that default trash-based `rm` from `rm --permanent`
/// (genuinely irreversible, no trash, no undo record) — `shoal-eval`'s
/// `builtin_effects()` discards CLI flags before deriving the effect, and
/// `Effect` (defined in `shoal-leash`, outside this crate) has no
/// `trashed`/`permanent` field to carry that distinction across the
/// boundary. A precise answer would need either a new field on `Effect`
/// itself, or the kernel inspecting raw AST flags at plan time — both bigger
/// changes than a effects-only reclassification. Between "call the common,
/// default, undoable `rm`/`mv` reversible" (which is what a cold agent
/// actually hit and correctly flagged as misleading) and "call the rare,
/// explicitly-opted-into `--permanent` case reversible too" (optimistic but
/// never claims an *opaque/external* delete is safe), this picks the former
/// as the least-misleading default given the information available here.
fn reversibility_from_effects(effects: &[Effect]) -> &'static str {
    let irreversible = effects.iter().any(|e| {
        matches!(
            e,
            Effect::Opaque | Effect::NetConnect { .. } | Effect::NetListen { .. }
        )
    });
    if irreversible {
        "irreversible"
    } else {
        "reversible"
    }
}

/// The `kind` tag an effect serializes with (`{"kind":"fs.write",…}`), used to
/// scope a `cap.request` grant to a set of effect kinds (site/content/internals/kernel-protocol.md).
fn effect_kind(effect: &Effect) -> String {
    serde_json::to_value(effect)
        .ok()
        .and_then(|v| v.get("kind").and_then(Json::as_str).map(String::from))
        .unwrap_or_default()
}

/// Normalize an effect kind so the agent-facing dotted convention (`fs.delete`,
/// per site/content/internals/kernel-protocol.md) matches the snake_case form the effect actually
/// serializes to (`fs_delete`).
fn norm_effect(kind: &str) -> String {
    kind.replace('.', "_")
}

/// The kernel's default elision thresholds, advertised at attach so a client
/// knows the budget before tightening/loosening per call (site/content/internals/kernel-protocol.md).
fn elide_defaults_json() -> Json {
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
fn approval_json(approval: Option<&ApprovalRecord>) -> Json {
    match approval {
        None => Json::Null,
        Some(a) => json!({
            "requester": a.requester,
            "approver": a.approver,
            "plan_ref": a.plan_ref,
            "scope": a.scope,
            "approved_at": a.approved_at_ns,
            "consumed_by": a.consumed_by,
        }),
    }
}

/// The `session.transcript` event payload for a new `out[n]` (see
/// `site/content/internals/kernel-protocol.md`): `{n, ref, summary:{type, ok?, cmd?, n?}}` — shape only, never payload.
fn transcript_event(value_ref: &Ref, value: &Value) -> Json {
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
fn approval_event(plan_ref: &str, effects: &[Json], principal: &str) -> Json {
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
fn journal_event(entry_id: i64, src: &str, ok: bool, principal: &str) -> Json {
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
fn render_event(value_ref: &Ref, render: &str) -> Json {
    json!({
        "$": "record",
        "v": {
            "ref": {"$":"str","v": value_ref.0},
            "render": {"$":"str","v": render},
        }
    })
}

/// Completion candidates at a cursor byte offset (the kernel `complete`
/// method). Keywords/builtins plus any `let`/`var`/`fn`/`alias` names declared
/// before the cursor, filtered by the partial word under the cursor.
fn complete_at(src: &str, cursor: usize) -> Vec<String> {
    const WORDS: &[&str] = &[
        "let", "var", "fn", "alias", "use", "export", "return", "break", "continue", "if", "else",
        "match", "for", "in", "while", "try", "catch", "true", "false", "null", "spawn", "with",
        "sh", "ls", "cd", "pwd", "cp", "mv", "rm", "mkdir", "cat", "echo", "run", "parallel",
        "pick", "interact", "explain",
    ];
    let before = &src[..cursor];
    // The partial identifier immediately left of the cursor.
    let start = before
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let partial = &before[start..];
    let mut names: Vec<String> = WORDS.iter().map(|s| s.to_string()).collect();
    // Declarations already in scope (`let x`, `fn y`, …).
    let toks: Vec<&str> = before
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|s| !s.is_empty())
        .collect();
    for pair in toks.windows(2) {
        if matches!(pair[0], "let" | "var" | "fn" | "alias") {
            names.push(pair[1].to_string());
        }
    }
    names.retain(|n| n.starts_with(partial));
    names.sort();
    names.dedup();
    names
}
unsafe fn libc_geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(
        writer: &mut UnixStream,
        reader: &mut BufReader<UnixStream>,
        id: i64,
        method: &str,
        params: Json,
    ) -> Response {
        write_frame(
            writer,
            &Request {
                jsonrpc: JSONRPC.into(),
                id: id.into(),
                method: method.into(),
                params,
            },
        )
        .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(reader, &mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    #[test]
    fn unix_stream_session_roundtrip() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        assert!(
            call(
                &mut client,
                &mut reader,
                1,
                "session.attach",
                json!({"client":{"kind":"test","tty":false}})
            )
            .error
            .is_none()
        );
        assert!(
            call(&mut client, &mut reader, 2, "parse", json!({"src":"1 + 2"}))
                .error
                .is_none()
        );
        let exec = call(&mut client, &mut reader, 3, "exec", json!({"src":"1 + 2"}));
        let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        let get = call(
            &mut client,
            &mut reader,
            4,
            "value.get",
            json!({"ref":value_ref,"path":null,"slice":null}),
        );
        assert_eq!(get.result.unwrap()["value"]["v"], 3);
        assert!(
            call(&mut client, &mut reader, 5, "task.list", json!({}))
                .error
                .is_none()
        );
        let journal = call(
            &mut client,
            &mut reader,
            6,
            "journal.query",
            json!({"limit":10}),
        );
        let entries = journal.result.unwrap();
        assert_eq!(entries[0]["src"], "1 + 2");
        assert_eq!(entries[0]["ok"], true);
        assert_eq!(
            entries[0]["opaque"], false,
            "pure arithmetic must not be journaled opaque:true"
        );
        assert!(
            entries[0]["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["kind"] == "value"
                    && o["len"].as_i64().unwrap() > 0
                    && o["hash"].as_str().unwrap().len() == 64)
        );
        let value_hash = entries[0]["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["kind"] == "value")
            .unwrap()["hash"]
            .as_str()
            .unwrap();
        let blob = kernel
            .journal
            .lock()
            .unwrap()
            .read_blob(value_hash)
            .unwrap()
            .unwrap();
        assert!(String::from_utf8(blob).unwrap().contains("\"v\":3"));
        // Slice applies to tables (list<record> semantically) — it used to
        // silently no-op and return the whole table — and slicing an
        // unordered/scalar value is an explicit error, not a silent identity.
        let texec = call(
            &mut client,
            &mut reader,
            40,
            "exec",
            json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
        );
        let table_ref = texec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        let sliced = call(
            &mut client,
            &mut reader,
            41,
            "value.get",
            json!({"ref":table_ref,"slice":[1,3]}),
        );
        let sliced = sliced.result.unwrap()["value"].clone();
        assert_eq!(sliced["$"], "table", "csv.parse yields a table: {sliced}");
        assert_eq!(sliced["n"], 2, "table slice should keep rows 1..3");
        let bad = call(
            &mut client,
            &mut reader,
            42,
            "value.get",
            json!({"ref":value_ref,"slice":[0,1]}),
        );
        assert_eq!(
            bad.error.expect("slicing an int must error").code,
            BAD_PATH_OR_SLICE,
            "slice on a scalar must be an explicit error"
        );
        // `[a..b]` path ranges (site/content/internals/kernel-protocol.md) — used to be "bad index".
        let ranged = call(
            &mut client,
            &mut reader,
            43,
            "value.get",
            json!({"ref":table_ref,"path":"rows[0..2]"}),
        );
        let ranged = ranged.result.unwrap()["value"].clone();
        assert_eq!(ranged["$"], "list", "rows[0..2]: {ranged}");
        assert_eq!(ranged["v"].as_array().unwrap().len(), 2);
        // `format=render` returns the human string; `format=raw` on a non-str
        // value is an explicit error.
        let rendered = call(
            &mut client,
            &mut reader,
            44,
            "value.get",
            json!({"ref":table_ref,"format":"render"}),
        );
        let rendered = rendered.result.unwrap();
        assert!(
            rendered["render"].as_str().unwrap().contains('1'),
            "render output: {rendered}"
        );
        let raw_bad = call(
            &mut client,
            &mut reader,
            45,
            "value.get",
            json!({"ref":value_ref,"format":"raw"}),
        );
        assert_eq!(
            raw_bad.error.expect("raw on int must error").code,
            BAD_PATH_OR_SLICE
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn leash_plan_approval_and_denial_flow() {
        for (opaque, expected, approvable) in
            [("ask", "approval_required", true), ("deny", "deny", false)]
        {
            let policy = Policy::from_toml(&format!(
                "[principal.\"{}\"]\nopaque='{opaque}'\nauto_apply='never'\n",
                principal()
            ))
            .unwrap();
            let kernel = Kernel::with_policy(policy);
            // Single-connection approve→apply flow: opt into self-ack (HR-D3);
            // the cross-principal separation gate is tested on its own.
            kernel.set_allow_self_ack(true);
            let (mut client, server) = UnixStream::pair().unwrap();
            let mut reader = BufReader::new(client.try_clone().unwrap());
            let server_kernel = kernel.clone();
            let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
            call(
                &mut client,
                &mut reader,
                1,
                "session.attach",
                json!({"client":{"kind":"agent","tty":false}}),
            );
            let planned = call(
                &mut client,
                &mut reader,
                2,
                "exec",
                json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
            );
            let result = planned.result.unwrap();
            assert_eq!(result["verdict"], expected);
            assert_eq!(result["effects"], json!([{"kind":"opaque"}]));
            let plan_ref = result["plan_ref"].as_str().unwrap();
            assert!(
                call(
                    &mut client,
                    &mut reader,
                    3,
                    "plan.apply",
                    json!({"plan_ref":plan_ref})
                )
                .error
                .is_some()
            );
            let grant = call(
                &mut client,
                &mut reader,
                4,
                "cap.request",
                json!({"plan_ref":plan_ref,"effects":[]}),
            );
            if approvable {
                assert!(grant.error.is_none());
                let applied = call(
                    &mut client,
                    &mut reader,
                    5,
                    "plan.apply",
                    json!({"plan_ref":plan_ref}),
                );
                let value = applied.result.unwrap()["value"].clone();
                assert_eq!(value["$"], "outcome");
                assert_eq!(value["ok"], true);
            } else {
                assert!(grant.error.is_some());
            }
            drop(client);
            drop(reader);
            thread.join().unwrap();
        }
    }

    /// Regression: `mode:"approved"` used to skip the leash verdict for ANY
    /// caller — the magic string alone bypassed policy. It is `plan.apply`'s
    /// re-entry and must name a stored plan that is approved for this
    /// session/principal with the same source; anything else is rejected
    /// even though a plain `run` of the same source would only be
    /// approval_required.
    #[test]
    fn approved_mode_is_not_a_caller_assertable_bypass() {
        let policy = Policy::from_toml(&format!(
            "[principal.\"{}\"]\nopaque='ask'\nauto_apply='never'\n",
            principal()
        ))
        .unwrap();
        let kernel = Kernel::with_policy(policy);
        // Approves its own plan over one connection to reach the approved
        // re-entry it is really testing: opt into self-ack (HR-D3).
        kernel.set_allow_self_ack(true);
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        // Baseline: the policy gates a plain run of this source.
        let run = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo hi }","mode":"run","position":"stmt"}),
        );
        assert_eq!(
            run.error.expect("run must be gated").code,
            APPROVAL_REQUIRED
        );
        // The bypass: bare `mode:"approved"` (no plan_ref) must be rejected…
        let bare = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"sh { echo hi }","mode":"approved","position":"stmt"}),
        );
        assert_eq!(
            bare.error.expect("bare approved must fail").code,
            LEASH_DENIED
        );
        // …as must a plan_ref that was never approved…
        let planned = call(
            &mut client,
            &mut reader,
            4,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
        );
        let plan_ref = planned.result.unwrap()["plan_ref"]
            .as_str()
            .unwrap()
            .to_owned();
        let unapproved = call(
            &mut client,
            &mut reader,
            5,
            "exec",
            json!({"src":"sh { echo hi }","mode":"approved","position":"stmt","plan_ref":plan_ref}),
        );
        assert_eq!(
            unapproved
                .error
                .expect("unapproved plan_ref must fail")
                .code,
            LEASH_DENIED
        );
        // …and an approved plan_ref may not smuggle DIFFERENT source.
        call(
            &mut client,
            &mut reader,
            6,
            "cap.request",
            json!({"plan_ref":plan_ref,"effects":[]}),
        );
        let smuggled = call(
            &mut client,
            &mut reader,
            7,
            "exec",
            json!({"src":"sh { rm -rf / }","mode":"approved","position":"stmt","plan_ref":plan_ref}),
        );
        assert_eq!(
            smuggled.error.expect("source smuggling must fail").code,
            LEASH_DENIED
        );
        // The sanctioned path still works: same source, approved plan.
        let sanctioned = call(
            &mut client,
            &mut reader,
            8,
            "exec",
            json!({"src":"sh { echo hi }","mode":"approved","position":"stmt","plan_ref":plan_ref}),
        );
        assert!(
            sanctioned.error.is_none(),
            "sanctioned approved exec failed: {:?}",
            sanctioned.error
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// Regression for the plan_ref collision (Plan identity used to hash only
    /// effects/reversibility/estimates, so any two opaque `sh { }` plans
    /// collided and `apply` silently ran whichever plan was last inserted).
    #[test]
    fn plan_refs_are_unique_per_source_and_apply_targets_the_right_one() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let plan_a = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo FIRST }","mode":"plan"}),
        )
        .result
        .unwrap();
        let plan_b = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"sh { echo SECOND }","mode":"plan"}),
        )
        .result
        .unwrap();
        let ref_a = plan_a["plan_ref"].as_str().unwrap().to_owned();
        let ref_b = plan_b["plan_ref"].as_str().unwrap().to_owned();
        assert_ne!(ref_a, ref_b, "distinct sources must not share a plan_ref");
        // Both plans are opaque (`sh { }`), so both need cap.request before
        // apply under the default permissive-but-opaque='allow' policy —
        // plan mode always requires explicit approval regardless of opaque
        // mode; grant both, then apply A and confirm it — not B — ran.
        call(
            &mut client,
            &mut reader,
            4,
            "cap.request",
            json!({"plan_ref":ref_a}),
        );
        call(
            &mut client,
            &mut reader,
            5,
            "cap.request",
            json!({"plan_ref":ref_b}),
        );
        let applied = call(
            &mut client,
            &mut reader,
            6,
            "plan.apply",
            json!({"plan_ref":ref_a}),
        );
        let out = applied.result.unwrap()["value"]["out"].clone();
        assert_eq!(out["$"], "str");
        assert_eq!(out["v"], "FIRST");
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn real_effects_not_opaque_for_pure_builtins() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        for src in ["1 + 2", "ls"] {
            let planned = call(
                &mut client,
                &mut reader,
                2,
                "exec",
                json!({"src":src,"mode":"plan"}),
            )
            .result
            .unwrap();
            assert_ne!(
                planned["effects"],
                json!([{"kind":"opaque"}]),
                "`{src}` must derive real effects, not the opaque fallback"
            );
            let exec = call(&mut client, &mut reader, 3, "exec", json!({"src":src}));
            let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
            let journal = call(
                &mut client,
                &mut reader,
                4,
                "journal.query",
                json!({"limit":1}),
            )
            .result
            .unwrap();
            assert_eq!(journal[0]["src"], src);
            assert_eq!(
                journal[0]["opaque"], false,
                "`{src}` must not be journaled opaque:true"
            );
            let _ = value_ref;
        }
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn value_get_path_traversal() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo hello world }"}),
        );
        let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        let out = call(
            &mut client,
            &mut reader,
            3,
            "value.get",
            json!({"ref":value_ref,"path":"out"}),
        );
        assert_eq!(
            out.result.unwrap()["value"],
            json!({"$":"str","v":"hello world"})
        );
        let ok = call(
            &mut client,
            &mut reader,
            4,
            "value.get",
            json!({"ref":value_ref,"path":"ok"}),
        );
        assert_eq!(ok.result.unwrap()["value"], json!({"$":"bool","v":true}));
        let bad = call(
            &mut client,
            &mut reader,
            5,
            "value.get",
            json!({"ref":value_ref,"path":"nope"}),
        );
        assert_eq!(bad.error.unwrap().code, BAD_PATH_OR_SLICE);

        let ls_exec = call(&mut client, &mut reader, 6, "exec", json!({"src":"ls"}));
        let ls_ref = ls_exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        let rows0 = call(
            &mut client,
            &mut reader,
            7,
            "value.get",
            json!({"ref":ls_ref,"path":"rows[0].name"}),
        );
        assert!(rows0.error.is_none(), "{:?}", rows0.error);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn exec_position_stmt_raises_value_does_not() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let stmt = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { exit 7 }","position":"stmt"}),
        );
        assert_eq!(stmt.error.unwrap().code, RAISED);
        let value = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"sh { exit 7 }","position":"value"}),
        );
        let result = value.result.unwrap();
        assert_eq!(result["value"]["$"], "outcome");
        assert_eq!(result["value"]["ok"], false);
        assert_eq!(result["value"]["status"], 7);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn async_tasks_survive_disconnect_and_cancel() {
        let kernel = Kernel::new();
        let (mut first, server) = UnixStream::pair().unwrap();
        let mut first_reader = BufReader::new(first.try_clone().unwrap());
        let k = kernel.clone();
        let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
        call(
            &mut first,
            &mut first_reader,
            1,
            "session.attach",
            json!({"session":"tasks","client":{"kind":"test","tty":false}}),
        );
        let started = call(
            &mut first,
            &mut first_reader,
            2,
            "exec",
            json!({"src":"sh { sleep 0.2 }","async":true}),
        );
        let survived: Ref =
            serde_json::from_value(started.result.unwrap()["task"].clone()).unwrap();
        drop(first);
        drop(first_reader);
        thread.join().unwrap();

        let (mut second, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(second.try_clone().unwrap());
        let k = kernel.clone();
        let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
        call(
            &mut second,
            &mut reader,
            3,
            "session.attach",
            json!({"session":"tasks","client":{"kind":"test","tty":false}}),
        );
        let awaited = call(
            &mut second,
            &mut reader,
            4,
            "task.await",
            json!({"task":survived}),
        );
        let awaited_value = awaited.result.unwrap();
        assert_eq!(awaited_value["state"], "completed", "{awaited_value}");
        let long = call(
            &mut second,
            &mut reader,
            5,
            "exec",
            json!({"src":"sh { sleep 30 }","async":true}),
        );
        let task: Ref = serde_json::from_value(long.result.unwrap()["task"].clone()).unwrap();
        let listed = call(&mut second, &mut reader, 6, "task.list", json!({}));
        assert!(listed.result.unwrap().as_array().unwrap().len() >= 2);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            call(
                &mut second,
                &mut reader,
                7,
                "task.cancel",
                json!({"task":task})
            )
            .error
            .is_none()
        );
        let before = Instant::now();
        let cancelled = call(
            &mut second,
            &mut reader,
            8,
            "task.await",
            json!({"task":task}),
        );
        assert!(before.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(cancelled.result.unwrap()["state"], "cancelled");
        drop(second);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn bearer_attach_uses_token_principal_and_rejects_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let mut tokens = TokenStore::open(dir.path().join("tokens.json")).unwrap();
        let (secret, _) = tokens
            .create(
                "agent:codex".into(),
                "readonly".into(),
                vec!["fs.read".into()],
                None,
            )
            .unwrap();
        drop(tokens);
        let kernel = Kernel::open(dir.path()).unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let k = kernel.clone();
        let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
        let attached = call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"token":secret,"client":{"kind":"agent","tty":false}}),
        );
        assert_eq!(attached.result.unwrap()["principal"], "agent:codex");
        let denied = call(
            &mut client,
            &mut reader,
            2,
            "session.attach",
            json!({"token":"not-a-token","client":{"kind":"agent","tty":false}}),
        );
        assert_eq!(denied.error.unwrap().code, AUTH_FAILED);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// End-to-end proof that a real (on-disk) kernel session's own
    /// `Evaluator` gets a journal installed automatically, with no test-side
    /// probe injection (unlike `handlers_exec::tests::
    /// exec_calls_set_source_so_stmt_journal_entries_carry_src`, which
    /// installs a journal by hand because it drives an ephemeral
    /// `Kernel::new()` — deliberately the ONE case that must stay journal-
    /// less, since it has no on-disk state dir at all). `Kernel::open` gives
    /// this kernel a real `state_dir`, so `session()` should now open a
    /// second journal handle on it and hand it to the session's evaluator
    /// (`crates/shoal-kernel/src/session.rs`). Attach, run a marker
    /// statement, then run the in-language `history` builtin — all over the
    /// same real Unix-socket wire `session.attach`/`exec` use in production —
    /// and confirm the marker's exact source text comes back in `history`'s
    /// `src` column. Before the fix, `session()` never called
    /// `Evaluator::set_journal` at all, so `history` inside a kernel session
    /// always came back empty regardless of what the kernel's own separate,
    /// coarser exec-level journal (`self.journal`, `journal.query`) recorded.
    #[test]
    fn kernel_open_installs_a_session_journal_so_history_builtin_sees_real_data() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = Kernel::open(dir.path()).unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let marker_src = "let kernel_journal_probe_4471 = 4471";
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src": marker_src}),
        );
        assert!(
            exec.error.is_none(),
            "marker exec must succeed: {:?}",
            exec.error
        );

        let hist = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src": "history"}),
        );
        let hist_result = hist.result.expect("exec of `history` must succeed");
        let cols = hist_result["value"]["cols"]["src"]
            .as_array()
            .unwrap_or_else(|| panic!("history's table has no src column: {hist_result:?}"));
        assert!(
            cols.iter().any(|v| v["v"] == marker_src),
            "no journal entry with src={marker_src:?} found among {cols:?} — the session \
             evaluator has no journal installed, so the in-language `history` builtin is inert \
             even though a real on-disk Kernel::open must install one automatically"
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// site/content/internals/kernel-protocol.md (`pty.list` / `shoal://pty`): open PTY sessions are
    /// first-class and session-scoped. A pty opened by session A is enumerated
    /// by A's `pty.list`, is invisible to a DIFFERENT session B (both the list
    /// and a direct `pty.read` of A's ref — an opaque `UNKNOWN_PTY`), and
    /// leaves A's list once closed. Drives a real `cat` on a PTY like the live
    /// MCP test, over the same Unix-socket wire production uses.
    #[test]
    fn pty_list_is_session_scoped() {
        let kernel = Kernel::new();
        // Session A on connection one.
        let (mut a, server) = UnixStream::pair().unwrap();
        let mut a_reader = BufReader::new(a.try_clone().unwrap());
        let ka = kernel.clone();
        let ta = std::thread::spawn(move || ka.handle_stream(server).unwrap());
        call(
            &mut a,
            &mut a_reader,
            1,
            "session.attach",
            json!({"session":"A","client":{"kind":"agent","tty":false}}),
        );
        let opened = call(&mut a, &mut a_reader, 2, "pty.open", json!({"cmd":"cat"}));
        let pty_id = opened.result.unwrap()["pty_id"]
            .as_str()
            .expect("pty.open returns a pty_id")
            .to_owned();

        // A sees exactly its one pty, with the documented shape.
        let list_a = call(&mut a, &mut a_reader, 3, "pty.list", json!({}));
        let ptys_a = list_a.result.unwrap()["ptys"].as_array().unwrap().clone();
        assert_eq!(ptys_a.len(), 1, "session A sees its one pty: {ptys_a:?}");
        assert_eq!(ptys_a[0]["pty_id"], json!(pty_id));
        assert_eq!(ptys_a[0]["cmd"], "cat");
        assert_eq!(ptys_a[0]["alive"], true);
        assert!(ptys_a[0]["pid"].as_u64().unwrap() > 0);
        assert!(ptys_a[0]["cols"].as_u64().unwrap() > 0);
        assert!(ptys_a[0]["rows"].as_u64().unwrap() > 0);

        // Session B on a second connection: a different session must NOT see
        // A's ptys, and cannot read A's pty by ref (opaque not-found).
        let (mut b, server) = UnixStream::pair().unwrap();
        let mut b_reader = BufReader::new(b.try_clone().unwrap());
        let kb = kernel.clone();
        let tb = std::thread::spawn(move || kb.handle_stream(server).unwrap());
        call(
            &mut b,
            &mut b_reader,
            1,
            "session.attach",
            json!({"session":"B","client":{"kind":"agent","tty":false}}),
        );
        let list_b = call(&mut b, &mut b_reader, 2, "pty.list", json!({}));
        assert!(
            list_b.result.unwrap()["ptys"]
                .as_array()
                .unwrap()
                .is_empty(),
            "session B must not see session A's ptys"
        );
        let read_b = call(
            &mut b,
            &mut b_reader,
            3,
            "pty.read",
            json!({"pty_id": pty_id}),
        );
        assert_eq!(
            read_b.error.expect("B cannot read A's pty").code,
            UNKNOWN_PTY,
            "another session's pty ref is an opaque UNKNOWN_PTY"
        );

        // Closing from A drops it out of A's list.
        call(
            &mut a,
            &mut a_reader,
            4,
            "pty.close",
            json!({"pty_id": pty_id}),
        );
        let list_a2 = call(&mut a, &mut a_reader, 5, "pty.list", json!({}));
        assert!(
            list_a2.result.unwrap()["ptys"]
                .as_array()
                .unwrap()
                .is_empty(),
            "a closed pty must leave pty.list"
        );

        drop(a);
        drop(a_reader);
        ta.join().unwrap();
        drop(b);
        drop(b_reader);
        tb.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // The elision rule (site/content/internals/kernel-protocol.md).
    // -----------------------------------------------------------------------

    /// A >100-row table (real `ls` over a directory with 150 files, not a
    /// synthetic stand-in) must come back elided: shape + schema + a 5-row
    /// preview, never the 150-row payload. Then drill into a single row by
    /// field-path and confirm that small result is NOT elided.
    #[test]
    fn big_table_exec_elides_then_drills_by_path() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..150 {
            std::fs::write(dir.path().join(format!("f{i:04}.txt")), b"x").unwrap();
        }
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src": format!("ls {}", dir.path().display())}),
        );
        let result = exec.result.expect("ls must succeed");
        let value_ref = result["ref"].as_str().unwrap().to_owned();
        // `ls` is a command: its wire shape is `outcome` with a structured
        // `.out`. Elision unwraps to `.out` for the decision (mirroring
        // render_block's outcome-unification) — the 150-row *table* elides,
        // the outer outcome envelope (status/ok/cmd/…) still travels.
        let value = &result["value"];
        assert_eq!(value["$"], "outcome");
        let out = &value["out"];
        assert_eq!(out["$"], "ref", "a 150-row table must elide, got {out}");
        assert_eq!(out["of"], "table");
        assert_eq!(out["n"], 150);
        assert_eq!(
            out["cols"]["name"], "str",
            "shape (schema) travels even when the payload does not"
        );
        assert_eq!(out["preview"]["$"], "table");
        assert_eq!(
            out["preview"]["n"], 5,
            "preview is a small head, not the full 150 rows"
        );
        assert!(out["render_head"].as_str().unwrap().contains("name"));
        let wire_len = serde_json::to_string(value).unwrap().len();
        assert!(
            wire_len < 4 * 1024,
            "the elided form itself must stay tiny, was {wire_len} bytes"
        );

        // Drill in: value.get with a field-path returns one small row —
        // NOT elided, because it never hits any threshold.
        let get = call(
            &mut client,
            &mut reader,
            3,
            "value.get",
            json!({"ref": value_ref, "path": "out[3]"}),
        );
        let drilled = get.result.unwrap()["value"].clone();
        assert_ne!(
            drilled["$"], "ref",
            "a single drilled row must not be elided: {drilled}"
        );
        assert_eq!(drilled["$"], "record");
        assert!(
            drilled["v"]["name"].is_object(),
            "drilled row keeps its fields: {drilled}"
        );

        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn small_value_is_not_elided() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"[1,2,3]"}),
        );
        let value = exec.result.unwrap()["value"].clone();
        assert_eq!(
            value["$"], "list",
            "a 3-item list is nowhere near any threshold"
        );
        assert_eq!(
            value["v"],
            json!([{"$":"int","v":1},{"$":"int","v":2},{"$":"int","v":3}])
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// A caller may loosen the byte budget, but never past the 64 KiB hard
    /// cap — a misbehaving agent cannot flood its own context by asking
    /// nicely.
    #[test]
    fn elision_hard_cap_cannot_be_disabled() {
        let huge = Value::Str("x".repeat(100_000));
        let loosened = ElideSpec {
            max_bytes: Some(5_000_000),
            max_rows: None,
            max_items: None,
        };
        let budget = ElideBudget::from_spec(Some(&loosened));
        assert_eq!(
            budget.max_bytes, ELIDE_HARD_CAP,
            "a requested budget above the hard cap must clamp down to it"
        );
        match elide_wire_value(&huge, "shoal://out/1", &budget) {
            WireValue::Ref { of, n, .. } => {
                assert_eq!(of, "str");
                assert_eq!(n, 100_000);
            }
            other => panic!(
                "a 100 KB string must still elide despite a 5 MB requested budget, got {other:?}"
            ),
        }
    }

    /// The flip side: loosening below the hard cap is honored, so a caller
    /// that wants a bit more headroom than the 8 KiB default legitimately
    /// gets it.
    #[test]
    fn elision_budget_can_be_loosened_up_to_the_hard_cap() {
        let modest = Value::Str("y".repeat(20_000)); // > 8 KiB default, < 64 KiB cap
        let loosened = ElideSpec {
            max_bytes: Some(5_000_000),
            max_rows: None,
            max_items: None,
        };
        let budget = ElideBudget::from_spec(Some(&loosened));
        match elide_wire_value(&modest, "shoal://out/1", &budget) {
            WireValue::Str { .. } => {}
            other => panic!("a 20 KiB string fits under a loosened 64 KiB cap, got {other:?}"),
        }
    }

    #[test]
    fn value_get_elide_param_tightens_default_row_threshold() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        // 10 items: under every default threshold, so a plain exec would not elide.
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"[0,1,2,3,4,5,6,7,8,9]"}),
        );
        assert_ne!(exec.result.as_ref().unwrap()["value"]["$"], "ref");
        let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        // A caller may tighten the budget per call — max_items:5 must elide
        // this same 10-item list on a follow-up `value.get`.
        let get = call(
            &mut client,
            &mut reader,
            3,
            "value.get",
            json!({"ref": value_ref, "path": null, "slice": null, "elide": {"max_items": 5}}),
        );
        let value = get.result.unwrap()["value"].clone();
        assert_eq!(
            value["$"], "ref",
            "a tightened per-call budget must elide: {value}"
        );
        assert_eq!(value["n"], 10);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// site/content/internals/language-conformance-contract.md wire follow-up: `value.get` RESOLVES a CAS-backed bytes ref. A
    /// top-level `CasBytes` (a value-position capture spilled to the CAS) elides
    /// to a `ref` on the default `format=json` path — a huge blob never ships
    /// whole — but an explicit `slice` or `format=raw` fetches the real content
    /// from the CAS through the value's own loader (the same `BytesLoad`/`Cas`
    /// seam), honoring the elision wall on what actually travels back.
    #[test]
    fn value_get_resolves_cas_backed_bytes_ref() {
        struct FixedLoader(Vec<u8>);
        impl shoal_value::BytesLoad for FixedLoader {
            fn load(&self) -> std::io::Result<Vec<u8>> {
                Ok(self.0.clone())
            }
        }
        struct FailLoader;
        impl shoal_value::BytesLoad for FailLoader {
            fn load(&self) -> std::io::Result<Vec<u8>> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "blob gone",
                ))
            }
        }
        let decode = |s: &str| {
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s).unwrap()
        };

        let kernel = Kernel::new();
        // Pre-populate a session transcript with a CAS-backed bytes value (5000
        // bytes — well over the raw budget, so the default fetch must elide) and
        // a second one whose loader fails (an unresolvable ref).
        let content: Vec<u8> = (0u32..5000).map(|i| (i % 251) as u8).collect();
        let session = kernel.session("casb", "human").unwrap();
        {
            let ok = std::sync::Arc::new(shoal_value::CasBytesVal {
                hash: "a".repeat(64),
                len: content.len() as u64,
                preview: std::sync::Arc::new(content[..64].to_vec()),
                truncated: false,
                loader: std::sync::Arc::new(FixedLoader(content.clone())),
            });
            let broken = std::sync::Arc::new(shoal_value::CasBytesVal {
                hash: "b".repeat(64),
                len: 123,
                preview: std::sync::Arc::new(Vec::new()),
                truncated: false,
                loader: std::sync::Arc::new(FailLoader),
            });
            let mut t = session.transcript.lock().unwrap();
            t.insert(Ref::new("out", 1u64), Value::CasBytes(ok));
            t.insert(Ref::new("out", 2u64), Value::CasBytes(broken));
        }

        let (mut client, mut reader, thread) = spawn(&kernel);
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"session":"casb","client":{"kind":"agent","tty":false}}),
        );

        // Default json: a CAS-backed value elides to an honest ref (no content),
        // carrying the TRUE length — a huge blob never ships whole.
        let def = call(
            &mut client,
            &mut reader,
            2,
            "value.get",
            json!({"ref":"out:1"}),
        );
        let v = def.result.unwrap()["value"].clone();
        assert_eq!(v["$"], "ref", "a CAS-backed value elides by default: {v}");
        assert_eq!(v["of"], "bytes");
        assert_eq!(
            v["n"],
            content.len(),
            "the elided ref carries the true length"
        );

        // A small slice RESOLVES to the exact CAS bytes, inline.
        let sl = call(
            &mut client,
            &mut reader,
            3,
            "value.get",
            json!({"ref":"out:1","slice":[0,10]}),
        );
        let v = sl.result.unwrap()["value"].clone();
        assert_eq!(v["$"], "bytes", "a small slice resolves inline: {v}");
        assert_eq!(decode(v["v"].as_str().unwrap()), content[0..10]);

        // format=raw resolves the FULL content (base64).
        let raw = call(
            &mut client,
            &mut reader,
            4,
            "value.get",
            json!({"ref":"out:1","format":"raw"}),
        );
        assert_eq!(
            decode(raw.result.unwrap()["raw_base64"].as_str().unwrap()),
            content
        );

        // slice + format=raw resolves exactly the requested sub-range.
        let rawslice = call(
            &mut client,
            &mut reader,
            5,
            "value.get",
            json!({"ref":"out:1","slice":[5,15],"format":"raw"}),
        );
        assert_eq!(
            decode(rawslice.result.unwrap()["raw_base64"].as_str().unwrap()),
            content[5..15]
        );

        // A slice that is itself still oversized re-elides at the wall.
        let big = call(
            &mut client,
            &mut reader,
            6,
            "value.get",
            json!({"ref":"out:1","slice":[0,5000]}),
        );
        assert_eq!(
            big.result.unwrap()["value"]["$"],
            "ref",
            "an oversized slice re-elides rather than shipping whole"
        );

        // An unresolvable ref (its CAS blob is gone) is a clear error, no panic.
        let bad = call(
            &mut client,
            &mut reader,
            7,
            "value.get",
            json!({"ref":"out:2","slice":[0,1]}),
        );
        assert!(
            bad.error.is_some(),
            "a failed CAS resolution surfaces an error, not a panic"
        );

        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// Read one already-written frame off the socket (no request sent) — for
    /// asserting on pushed `event` notifications interleaved with responses.
    fn recv_line(reader: &mut BufReader<UnixStream>) -> Json {
        let mut line = String::new();
        std::io::BufRead::read_line(reader, &mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn attach(client: &mut UnixStream, reader: &mut BufReader<UnixStream>) -> Response {
        call(
            client,
            reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        )
    }

    fn spawn(
        kernel: &Arc<Kernel>,
    ) -> (
        UnixStream,
        BufReader<UnixStream>,
        std::thread::JoinHandle<()>,
    ) {
        let (client, server) = UnixStream::pair().unwrap();
        let reader = BufReader::new(client.try_clone().unwrap());
        let k = kernel.clone();
        let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
        (client, reader, thread)
    }

    // -----------------------------------------------------------------------
    // Events — channels, cursors, push (site/content/internals/kernel-protocol.md).
    // -----------------------------------------------------------------------

    #[test]
    fn events_publish_read_roundtrips_on_a_user_channel() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        // Only user.* channels are client-writable.
        let denied = call(
            &mut client,
            &mut reader,
            2,
            "events.publish",
            json!({"channel":"session.transcript","payload":{"$":"int","v":1}}),
        );
        assert_eq!(denied.error.unwrap().code, INVALID_PARAMS);
        // Publish two values, then read them back with monotonic per-channel seq.
        for (i, v) in ["go", "stop"].iter().enumerate() {
            let published = call(
                &mut client,
                &mut reader,
                3 + i as i64,
                "events.publish",
                json!({"channel":"user.deploy","payload":{"$":"str","v":v}}),
            );
            assert_eq!(published.result.unwrap()["seq"], i as i64);
        }
        let read = call(
            &mut client,
            &mut reader,
            9,
            "events.read",
            json!({"channel":"user.deploy"}),
        );
        let events = read.result.unwrap()["events"].clone();
        let events = events.as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["payload"], json!({"$":"str","v":"go"}));
        assert_eq!(events[1]["seq"], 1);
        // Cursor read: since=0 returns only events after seq 0.
        let tail = call(
            &mut client,
            &mut reader,
            10,
            "events.read",
            json!({"channel":"user.deploy","since":0}),
        );
        let tail = tail.result.unwrap()["events"].clone();
        assert_eq!(tail.as_array().unwrap().len(), 1);
        assert_eq!(tail[0]["payload"], json!({"$":"str","v":"stop"}));
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// The `journal` channel is journal-backed, as required by
    /// `site/content/internals/kernel-protocol.md`:
    /// and replayable from ANY seq, not just the last `EVENT_RING_CAP` events.
    /// Generate more than a full ring of journal-backed events (one finished
    /// journal entry — hence one `journal` event — per exec), then read from a
    /// `since` that has aged out of the in-memory ring and assert every aged-out
    /// event comes back with the correct seq, correct scoping, and contiguous
    /// with the events the ring still holds. Also pins the not-found (since
    /// beyond newest) case and that the in-ring fast path is untouched.
    #[test]
    fn journal_channel_replays_aged_out_events_from_the_journal() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);

        // One `journal` event per exec; overflow the ring by a clear margin.
        let total = EVENT_RING_CAP + 40;
        for i in 0..total {
            let r = call(
                &mut client,
                &mut reader,
                100 + i as i64,
                "exec",
                json!({"src":"1 + 1"}),
            );
            assert!(r.error.is_none(), "exec {i} failed: {:?}", r.error);
        }

        // seq 0 has aged out of the ring (only the last EVENT_RING_CAP seqs are
        // still retained). Reading since=0 must still return every event after
        // seq 0 — the aged-out ones rebuilt from the journal, then the ring
        // tail — contiguous across the ring boundary.
        let read = call(
            &mut client,
            &mut reader,
            9001,
            "events.read",
            json!({"channel":"journal","since":0}),
        );
        let events = read.result.unwrap()["events"].as_array().unwrap().clone();
        assert_eq!(
            events.len(),
            total - 1,
            "since=0 (exclusive) must return every journal event after seq 0 — ring + journal \
             fallback, not just the ring's {EVENT_RING_CAP}"
        );
        for (idx, ev) in events.iter().enumerate() {
            let expected_seq = (idx + 1) as u64;
            assert_eq!(
                ev["seq"].as_u64().unwrap(),
                expected_seq,
                "seqs must be contiguous and ascending across the ring boundary: {ev}"
            );
            assert_eq!(ev["channel"], "journal");
            let payload = &ev["payload"]["v"];
            assert_eq!(payload["ok"]["v"], true, "each `1 + 1` finished ok: {ev}");
            assert_eq!(
                payload["head"]["v"], "1",
                "head is the leading command word of the entry: {ev}"
            );
            assert_eq!(
                payload["principal"]["v"],
                principal(),
                "reconstructed events keep the live channel's principal scoping: {ev}"
            );
            // In-memory kernel: no evaluator double-journaling, so the coarse
            // exec entry's rowid is exactly seq+1 (first append == rowid 1 ==
            // seq 0). Pins the seq↔journal-entry correspondence.
            assert_eq!(
                payload["entry_id"]["v"].as_u64().unwrap(),
                expected_seq + 1,
                "seq↔entry_id correspondence: {ev}"
            );
        }

        // A `limit` still bounds the result to the newest N even when the
        // fallback contributed older events.
        let limited = call(
            &mut client,
            &mut reader,
            9002,
            "events.read",
            json!({"channel":"journal","since":0,"limit":5}),
        );
        let limited = limited.result.unwrap()["events"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(limited.len(), 5, "limit keeps the newest 5");
        assert_eq!(limited[4]["seq"].as_u64().unwrap(), (total - 1) as u64);

        // Not-found / beyond-newest: since at or past the newest seq is empty,
        // never an error.
        let last_seq = (total - 1) as u64;
        let at_newest = call(
            &mut client,
            &mut reader,
            9003,
            "events.read",
            json!({"channel":"journal","since": last_seq}),
        );
        assert!(
            at_newest.result.unwrap()["events"]
                .as_array()
                .unwrap()
                .is_empty(),
            "since == newest seq: nothing after it"
        );
        let beyond = call(
            &mut client,
            &mut reader,
            9004,
            "events.read",
            json!({"channel":"journal","since": last_seq + 500}),
        );
        assert!(
            beyond.result.unwrap()["events"]
                .as_array()
                .unwrap()
                .is_empty(),
            "since beyond newest seq: empty, not an error"
        );

        // Fast path untouched: a since WITHIN the ring is served from the ring
        // alone and returns exactly the tail after it.
        let within = last_seq - 3;
        let tail = call(
            &mut client,
            &mut reader,
            9005,
            "events.read",
            json!({"channel":"journal","since": within}),
        );
        let tail = tail.result.unwrap()["events"].as_array().unwrap().clone();
        assert_eq!(tail.len(), 3, "since 3 below newest: exactly the last 3");
        assert_eq!(tail[0]["seq"].as_u64().unwrap(), within + 1);

        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// The journal-backed replay must reflect the `journal` CHANNEL, not every
    /// row in the journal store. In an on-disk session the session evaluator
    /// ALSO writes its own finer per-statement entries into the same store
    /// (`session.rs`), but only the coarse exec-level entry fires a `journal`
    /// event. Reconstruction keys off the seq↔entry index, so those
    /// per-statement rows are excluded — the replay has exactly one event per
    /// exec, not one per statement, with no phantom events and no leakage.
    #[test]
    fn journal_channel_replay_excludes_evaluator_per_statement_entries() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = Kernel::open(dir.path()).unwrap();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);

        // Three top-level statements per exec: on-disk, the store gets one
        // coarse entry + three per-statement entries per exec, but the channel
        // fires once per exec. Overflow the ring so replay must hit the journal.
        let execs = EVENT_RING_CAP + 5;
        for i in 0..execs {
            let r = call(
                &mut client,
                &mut reader,
                100 + i as i64,
                "exec",
                json!({"src":"let a = 1\nlet b = 2\na + b"}),
            );
            assert!(r.error.is_none(), "exec {i} failed: {:?}", r.error);
        }

        // Sanity: the store itself holds far more rows than the channel
        // published (the evaluator's per-statement entries), so a naive
        // "reconstruct every journal row" would over-produce.
        let jq = call(
            &mut client,
            &mut reader,
            9000,
            "journal.query",
            json!({"limit": 1_000_000}),
        );
        let rows = jq.result.unwrap().as_array().unwrap().len();
        assert!(
            rows >= execs * 3,
            "on-disk store should also hold the finer per-statement entries: {rows} rows for \
             {execs} execs"
        );

        // Replay from seq 0 (aged out): EXACTLY one event per exec, contiguous
        // seqs, each the coarse whole-submission entry (head "let") — no
        // phantom per-statement events.
        let read = call(
            &mut client,
            &mut reader,
            9001,
            "events.read",
            json!({"channel":"journal","since":0}),
        );
        let events = read.result.unwrap()["events"].as_array().unwrap().clone();
        assert_eq!(
            events.len(),
            execs - 1,
            "one journal event per exec, not per statement — evaluator rows are filtered out"
        );
        for (idx, ev) in events.iter().enumerate() {
            assert_eq!(
                ev["seq"].as_u64().unwrap(),
                (idx + 1) as u64,
                "contiguous seqs, no per-statement phantoms: {ev}"
            );
            assert_eq!(
                ev["payload"]["v"]["head"]["v"], "let",
                "each replayed event is the coarse exec-level entry: {ev}"
            );
        }

        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// `site/content/internals/kernel-protocol.md` also requires `session.transcript`
    /// is now ALSO journal-backed and replayable from ANY seq, mirroring the
    /// `journal` channel's replay (`read_transcript_channel`/
    /// `reconstruct_transcript_events` in `eventbus.rs`, backed by
    /// `shoal_journal::Journal::record_transcript_event`/
    /// `transcript_events_by_entry`). Generate more than a full ring of
    /// transcript events (one per successful exec), read from a `since` that
    /// has aged out of the ring, and assert every aged-out event comes back
    /// with the correct seq and the exact persisted payload, contiguous with
    /// whatever the ring still holds. Also pins the `limit`/beyond-newest/
    /// within-ring cases, same as the `journal` channel's test.
    #[test]
    fn session_transcript_channel_replays_aged_out_events_from_the_journal() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);

        // One `session.transcript` event per exec (every exec here succeeds);
        // overflow the ring by a clear margin.
        let total = EVENT_RING_CAP + 40;
        for i in 0..total {
            let r = call(
                &mut client,
                &mut reader,
                100 + i as i64,
                "exec",
                json!({"src":"1 + 1"}),
            );
            assert!(r.error.is_none(), "exec {i} failed: {:?}", r.error);
        }

        // seq 0 has aged out of the ring. Reading since=0 must still return
        // every event after seq 0 — the aged-out ones rebuilt from the
        // journal's `transcript_event` table, then the ring tail —
        // contiguous across the ring boundary.
        let read = call(
            &mut client,
            &mut reader,
            9001,
            "events.read",
            json!({"channel":"session.transcript","since":0}),
        );
        let events = read.result.unwrap()["events"].as_array().unwrap().clone();
        assert_eq!(
            events.len(),
            total - 1,
            "since=0 (exclusive) must return every transcript event after seq 0 — ring + \
             journal fallback, not just the ring's {EVENT_RING_CAP}"
        );
        for (idx, ev) in events.iter().enumerate() {
            let expected_seq = (idx + 1) as u64;
            assert_eq!(
                ev["seq"].as_u64().unwrap(),
                expected_seq,
                "seqs must be contiguous and ascending across the ring boundary: {ev}"
            );
            assert_eq!(ev["channel"], "session.transcript");
            let payload = &ev["payload"]["v"];
            // Every exec here produces exactly one out[n], so seq N's ref is
            // out:(N+2): seq 0 is out:1 (excluded by since=0), so the first
            // returned event (seq 1) is out:2.
            assert_eq!(
                payload["ref"]["v"],
                format!("out:{}", expected_seq + 1),
                "reconstructed ref must match the live numbering: {ev}"
            );
            assert_eq!(
                payload["summary"]["v"]["type"]["v"], "int",
                "reconstructed summary must reflect the value's real shape, not a placeholder: \
                 {ev}"
            );
        }

        // A `limit` still bounds the result to the newest N even when the
        // fallback contributed older events.
        let limited = call(
            &mut client,
            &mut reader,
            9002,
            "events.read",
            json!({"channel":"session.transcript","since":0,"limit":5}),
        );
        let limited = limited.result.unwrap()["events"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(limited.len(), 5, "limit keeps the newest 5");
        assert_eq!(limited[4]["seq"].as_u64().unwrap(), (total - 1) as u64);

        // Not-found / beyond-newest: since at or past the newest seq is
        // empty, never an error.
        let last_seq = (total - 1) as u64;
        let at_newest = call(
            &mut client,
            &mut reader,
            9003,
            "events.read",
            json!({"channel":"session.transcript","since": last_seq}),
        );
        assert!(
            at_newest.result.unwrap()["events"]
                .as_array()
                .unwrap()
                .is_empty(),
            "since == newest seq: nothing after it"
        );
        let beyond = call(
            &mut client,
            &mut reader,
            9004,
            "events.read",
            json!({"channel":"session.transcript","since": last_seq + 500}),
        );
        assert!(
            beyond.result.unwrap()["events"]
                .as_array()
                .unwrap()
                .is_empty(),
            "since beyond newest seq: empty, not an error"
        );

        // Fast path untouched: a since WITHIN the ring is served from the
        // ring alone and returns exactly the tail after it.
        let within = last_seq - 3;
        let tail = call(
            &mut client,
            &mut reader,
            9005,
            "events.read",
            json!({"channel":"session.transcript","since": within}),
        );
        let tail = tail.result.unwrap()["events"].as_array().unwrap().clone();
        assert_eq!(tail.len(), 3, "since 3 below newest: exactly the last 3");
        assert_eq!(tail[0]["seq"].as_u64().unwrap(), within + 1);

        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// The `session.transcript` channel fires only on a SUCCESSFUL exec
    /// (`handlers_exec.rs`'s error path never reaches `publish_transcript`);
    /// a failed exec still consumes an `out[n]` slot and gets its own coarse
    /// journal entry, but no `transcript_event` row. Journal-backed replay
    /// must reflect exactly that — entries with no transcript row are simply
    /// absent from the replayed channel, never phantom events — while
    /// staying contiguous and in order for the entries that DO have one, the
    /// same "reconstruction reflects the channel, not the whole store"
    /// property `journal_channel_replay_excludes_evaluator_per_statement_
    /// entries` pins for the `journal` channel.
    #[test]
    fn session_transcript_channel_replay_skips_entries_with_no_transcript_row() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);

        // Overflow the ring by a clear margin even after excluding the
        // ~third of execs that fail (and thus never fire a transcript
        // event): every exec still consumes an out[n] slot and a journal
        // entry, so the store ends up with MORE entries than transcript
        // events, mirroring the journal channel's over-production hazard.
        let total = EVENT_RING_CAP * 2;
        let mut expected_refs = Vec::new();
        for i in 0..total {
            let fails = i % 3 == 0;
            let src = if fails { "1 / 0" } else { "1 + 1" };
            let r = call(
                &mut client,
                &mut reader,
                100 + i as i64,
                "exec",
                json!({"src": src}),
            );
            let n = i + 1; // out[n] numbering: consumed by every exec, pass or fail
            if fails {
                assert!(r.error.is_some(), "exec {i} ({src}) should have failed");
            } else {
                assert!(r.error.is_none(), "exec {i} failed: {:?}", r.error);
                expected_refs.push(format!("out:{n}"));
            }
        }
        assert!(
            expected_refs.len() > EVENT_RING_CAP,
            "the mix must still overflow the transcript ring: {} successes",
            expected_refs.len()
        );

        let read = call(
            &mut client,
            &mut reader,
            9001,
            "events.read",
            json!({"channel":"session.transcript","since":0}),
        );
        let events = read.result.unwrap()["events"].as_array().unwrap().clone();
        // since=0 excludes seq 0 itself (the first successful exec's own
        // transcript event) — every OTHER successful exec's event must
        // appear, in order, contiguous, with none for a failed exec.
        let expected = &expected_refs[1..];
        assert_eq!(
            events.len(),
            expected.len(),
            "exactly one event per SUCCESSFUL exec after seq 0 — no phantoms for the failed ones"
        );
        for (idx, (ev, want_ref)) in events.iter().zip(expected.iter()).enumerate() {
            assert_eq!(
                ev["seq"].as_u64().unwrap(),
                (idx + 1) as u64,
                "contiguous seqs across the ring boundary: {ev}"
            );
            assert_eq!(&ev["payload"]["v"]["ref"]["v"], want_ref);
        }

        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// Event-bus seq state for BOTH
    /// journal-backed channels must survive a real kernel RESTART, not just
    /// live within one process's lifetime. Before this fix, `Kernel::open`
    /// always built a brand-new, unseeded `EventBus::default()` — so
    /// reopening an EXISTING on-disk store reset both channels' `next_seq`
    /// to 0 and their durable indexes to empty, even though the store itself
    /// still held every entry from the prior process. A reconnecting agent's
    /// persisted `since=N` cursor would then get an empty read, and the
    /// freshly-restarted kernel's own next publish would collide with
    /// whatever seq 0 meant in the PRIOR lifetime.
    ///
    /// Simulates a restart with two SEPARATE `Kernel::open` calls against the
    /// same on-disk state dir, one after the other (never both alive at
    /// once — a real process restart, not two concurrent kernels sharing a
    /// store). The "prior lifetime" execs are 3-statement programs, exactly
    /// like `journal_channel_replay_excludes_evaluator_per_statement_entries`,
    /// so the on-disk store also holds the session evaluator's own
    /// per-statement rows — proving the seeded index still reflects the
    /// CHANNEL after a restart, not every row in the store.
    #[test]
    fn event_bus_seq_state_survives_a_kernel_restart() {
        let dir = tempfile::tempdir().unwrap();
        let pre_restart_execs = 5usize;

        // "Prior lifetime": open, run a few execs, then drop the kernel
        // entirely (closing its journal handle) before reopening.
        {
            let kernel = Kernel::open(dir.path()).unwrap();
            let (mut client, mut reader, thread) = spawn(&kernel);
            attach(&mut client, &mut reader);
            for i in 0..pre_restart_execs {
                let r = call(
                    &mut client,
                    &mut reader,
                    100 + i as i64,
                    "exec",
                    json!({"src":"let a = 1\nlet b = 2\na + b"}),
                );
                assert!(r.error.is_none(), "prelude exec {i} failed: {:?}", r.error);
            }
            drop(client);
            drop(reader);
            thread.join().unwrap();
        }

        // Sanity: the on-disk store holds more rows than execs (the
        // evaluator's own per-statement entries are in there too).
        let total_rows = Journal::open(dir.path())
            .unwrap()
            .query(&JournalQuery {
                limit: 1_000_000,
                ..Default::default()
            })
            .unwrap()
            .len();
        assert!(
            total_rows > pre_restart_execs,
            "the on-disk store should also hold per-statement rows: {total_rows} rows for \
             {pre_restart_execs} execs"
        );

        // "Restart": a brand-new `Kernel::open` (fresh `EventBus::default()`)
        // against the exact same on-disk state dir.
        let kernel2 = Kernel::open(dir.path()).unwrap();
        let (mut client2, mut reader2, thread2) = spawn(&kernel2);
        attach(&mut client2, &mut reader2);

        // A newly published `journal` event must continue past the
        // pre-existing (coarse) journal-channel entry count — never reset
        // to 0 and never collide with a pre-restart seq.
        let exec = call(
            &mut client2,
            &mut reader2,
            200,
            "exec",
            json!({"src":"1 + 1"}),
        );
        assert!(
            exec.error.is_none(),
            "post-restart exec failed: {:?}",
            exec.error
        );

        let read_after = call(
            &mut client2,
            &mut reader2,
            201,
            "events.read",
            json!({"channel":"journal","since":0}),
        );
        let events_after = read_after.result.unwrap()["events"]
            .as_array()
            .unwrap()
            .clone();
        let newest_seq = events_after
            .last()
            .expect("at least the just-published post-restart event")["seq"]
            .as_u64()
            .unwrap();
        assert!(
            newest_seq >= pre_restart_execs as u64,
            "(a) a newly-published journal seq ({newest_seq}) must continue past the \
             {pre_restart_execs} pre-restart journal entries, not reset to 0: {events_after:?}"
        );

        // (b) The pre-restart journal events are still replayable: reading
        // since=0 (aged out of the brand-new, empty ring) reconstructs them
        // from the durable journal — exactly `pre_restart_execs - 1` of them
        // (since=0 excludes seq 0 itself), each the coarse whole-submission
        // entry, not a per-statement phantom.
        let pre_restart_events: Vec<&Json> = events_after
            .iter()
            .filter(|e| e["seq"].as_u64().unwrap() < pre_restart_execs as u64)
            .collect();
        assert_eq!(
            pre_restart_events.len(),
            pre_restart_execs - 1,
            "replay after restart must recover exactly the pre-restart journal events, no more \
             (per-statement rows) and no fewer: {events_after:?}"
        );
        for ev in &pre_restart_events {
            assert_eq!(
                ev["payload"]["v"]["head"]["v"], "let",
                "a reconstructed pre-restart event must be the coarse entry, not a \
                 per-statement row: {ev}"
            );
        }

        // (c) Same replay-survives-restart property for `session.transcript`
        // — every pre-restart exec here succeeded, so each has a persisted
        // transcript row too.
        let transcript_read = call(
            &mut client2,
            &mut reader2,
            202,
            "events.read",
            json!({"channel":"session.transcript","since":0}),
        );
        let transcript_events = transcript_read.result.unwrap()["events"]
            .as_array()
            .unwrap()
            .clone();
        let pre_restart_transcript: Vec<&Json> = transcript_events
            .iter()
            .filter(|e| e["seq"].as_u64().unwrap() < pre_restart_execs as u64)
            .collect();
        assert_eq!(
            pre_restart_transcript.len(),
            pre_restart_execs - 1,
            "replay after restart must recover the pre-restart transcript events too: \
             {transcript_events:?}"
        );

        drop(client2);
        drop(reader2);
        thread2.join().unwrap();
    }

    /// Companion to the restart test above: a brand-new on-disk store (the
    /// common case — most `Kernel::open` calls are not reopening a
    /// previously used store) must still start both journal-backed
    /// channels' seqs at 0, exactly as before this fix.
    #[test]
    fn kernel_open_on_a_fresh_store_still_starts_journal_channel_seqs_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = Kernel::open(dir.path()).unwrap();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let exec = call(&mut client, &mut reader, 1, "exec", json!({"src":"1 + 1"}));
        assert!(exec.error.is_none());
        let read = call(
            &mut client,
            &mut reader,
            2,
            "events.read",
            json!({"channel":"journal","since":null}),
        );
        let events = read.result.unwrap()["events"].as_array().unwrap().clone();
        assert_eq!(
            events[0]["seq"], 0,
            "a fresh on-disk store's first journal event must still start at seq 0: {events:?}"
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn subscribe_pushes_session_transcript_event_before_the_exec_response() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        call(
            &mut client,
            &mut reader,
            2,
            "events.subscribe",
            json!({"channel":"session.transcript"}),
        );
        write_frame(
            &mut client,
            &Request {
                jsonrpc: JSONRPC.into(),
                id: 3.into(),
                method: "exec".into(),
                params: json!({"src":"1 + 2"}),
            },
        )
        .unwrap();
        // Both the pushed `session.transcript` notification and the exec
        // response land on this connection, but which arrives FIRST is no
        // longer guaranteed (see `site/content/internals/kernel-protocol.md`): the notification is
        // now delivered by a dedicated per-subscriber writer thread, off the
        // dispatch call path entirely, so `publish()` never blocks on a
        // slow/stalled subscriber's socket. That decoupling is exactly what
        // makes the ordering this test used to pin (event strictly before
        // response, because the old code wrote the notification inline,
        // synchronously, from within dispatch) impossible to promise anymore
        // — read both frames and check each on its own merits, regardless of
        // which arrives first.
        let first = recv_line(&mut reader);
        let second = recv_line(&mut reader);
        let (note, resp) = if first["method"] == "event" {
            (first, second)
        } else {
            (second, first)
        };
        assert_eq!(note["method"], "event", "expected a pushed event: {note}");
        assert_eq!(note["params"]["channel"], "session.transcript");
        assert_eq!(note["params"]["payload"]["v"]["ref"]["v"], "out:1");
        assert_eq!(resp["id"], 3);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// `approval` used to be advertised in
    /// `STATIC_CHANNELS` with nothing ever publishing to it — a dead
    /// channel. `exec {mode:"plan"}` now fires it the moment a plan lands at
    /// `Verdict::ApprovalRequired`, so a SEPARATE principal (a human's
    /// session, a supervising agent — a different connection here, on
    /// purpose) learns about a pending approval by subscribing, never by
    /// polling `journal.query` or re-deriving the same plan.
    #[test]
    fn approval_channel_fires_when_a_plan_needs_approval() {
        let policy = Policy::from_toml(&format!(
            "[principal.\"{}\"]\nopaque='ask'\nauto_apply='never'\n",
            principal()
        ))
        .unwrap();
        let kernel = Kernel::with_policy(policy);

        // A separate observer connection, subscribed to `approval`, never
        // itself issuing the plan below.
        let (mut observer, mut observer_reader, observer_thread) = spawn(&kernel);
        attach(&mut observer, &mut observer_reader);
        call(
            &mut observer,
            &mut observer_reader,
            2,
            "events.subscribe",
            json!({"channel":"approval"}),
        );

        // A different connection: the agent whose plan lands at
        // approval_required.
        let (mut agent, mut agent_reader, agent_thread) = spawn(&kernel);
        attach(&mut agent, &mut agent_reader);
        let planned = call(
            &mut agent,
            &mut agent_reader,
            2,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
        )
        .result
        .unwrap();
        assert_eq!(planned["verdict"], "approval_required");
        assert_eq!(planned["approval_pending"], true);
        let plan_ref = planned["plan_ref"].as_str().unwrap().to_owned();

        // The observer receives the approval event on its own connection —
        // it never touched this plan itself.
        let note = recv_line(&mut observer_reader);
        assert_eq!(note["method"], "event", "expected a pushed event: {note}");
        assert_eq!(note["params"]["channel"], "approval");
        let payload = &note["params"]["payload"]["v"];
        assert_eq!(payload["plan_ref"]["v"], plan_ref);
        assert_eq!(payload["principal"]["v"], principal());
        assert_eq!(payload["effects"]["v"], json!([{"kind":"opaque"}]));
        assert_eq!(
            payload["expires"]["$"], "null",
            "no plan-expiry mechanism exists yet — honestly null, not fabricated: {payload}"
        );

        drop(observer);
        drop(observer_reader);
        observer_thread.join().unwrap();
        drop(agent);
        drop(agent_reader);
        agent_thread.join().unwrap();
    }

    /// A plan that is immediately `Verdict::Allow` never needed approval, so
    /// it must NOT fire `approval` — only a plan actually stuck pending
    /// should ever announce on this channel.
    #[test]
    fn approval_channel_stays_silent_for_an_immediately_allowed_plan() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        call(
            &mut client,
            &mut reader,
            2,
            "events.subscribe",
            json!({"channel":"approval"}),
        );
        // The default-permissive policy allows pure arithmetic outright.
        let planned = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"1 + 2","mode":"plan"}),
        )
        .result
        .unwrap();
        assert_eq!(planned["verdict"], "allow");
        let read_back = call(
            &mut client,
            &mut reader,
            4,
            "events.read",
            json!({"channel":"approval"}),
        )
        .result
        .unwrap();
        assert_eq!(
            read_back["events"].as_array().unwrap().len(),
            0,
            "an allowed plan must never announce on `approval`: {read_back}"
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn attach_advertises_channels_elide_defaults_and_enforcement() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        let r = attach(&mut client, &mut reader).result.unwrap();
        assert_eq!(r["caps_enforced"], false);
        assert_eq!(r["elide_defaults"]["max_rows"], 100);
        assert_eq!(r["elide_defaults"]["hard_cap"], 64 * 1024);
        assert!(
            r["channels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "session.transcript")
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn attach_reports_the_honest_detected_tier() {
        // site/content/internals/language-conformance-contract.md tier honesty: the tier at attach is the strongest OS backend
        // this host actually has (detected), NOT a hardcoded "D". Under the
        // default-permissive human policy nothing is confined, so `enforced`
        // stays false even where a backend exists.
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        let r = attach(&mut client, &mut reader).result.unwrap();
        let expected = tier_letter(EnforcementStatus::detect().available_tier);
        assert_eq!(r["caps"]["tier"], expected);
        assert_eq!(r["caps_enforced"], false);
        assert_eq!(r["caps"]["enforced"], false);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn attach_enforces_only_for_a_scoped_principal_with_a_real_backend() {
        // A genuinely-scoped principal reports `enforced: true` — but only when
        // a real OS backend (Landlock/Seatbelt) exists; on a host without one
        // the answer honestly degrades to false rather than claiming a wall
        // that isn't there.
        let who = principal();
        let policy = Policy::from_toml(&format!(
            "[principal.\"{who}\"]\nopaque='allow'\nauto_apply='in-grant'\n\n\
             [principal.\"{who}\".fs]\nread=[\"/usr/**\"]\n"
        ))
        .unwrap();
        let kernel = Kernel::with_policy(policy);
        let (mut client, mut reader, thread) = spawn(&kernel);
        let r = attach(&mut client, &mut reader).result.unwrap();
        let status = EnforcementStatus::detect();
        let backend_present = matches!(
            status.available_tier,
            EnforcementTier::A | EnforcementTier::C
        );
        assert_eq!(r["caps_enforced"], backend_present);
        assert_eq!(r["caps"]["enforced"], backend_present);
        assert_eq!(r["caps"]["tier"], tier_letter(status.available_tier));
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // Plan reversibility (site/content/internals/kernel-protocol.md) — derived, not hardcoded.
    // -----------------------------------------------------------------------

    #[test]
    fn plan_reversibility_is_derived_from_effects() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        // shoal's `rm` is trash-based (journaled, `apply` fully recovers it)
        // — NOT an opaque, unrecoverable delete, so a plan for it must not
        // be flatly reported "irreversible" (bug: a cold agent driving the
        // MCP surface found this misleading).
        let del = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"rm doomed.txt","mode":"plan"}),
        )
        .result
        .unwrap();
        assert_eq!(
            del["reversibility"], "reversible",
            "shoal's rm trashes (journaled undo) rather than deleting outright: {del}"
        );
        let pure = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"1 + 2","mode":"plan"}),
        )
        .result
        .unwrap();
        assert_eq!(pure["reversibility"], "reversible");
        // An opaque external command is a DIFFERENT effect (`Effect::Opaque`,
        // never `Effect::FsDelete`) and must stay irreversible even when its
        // source text also happens to say "rm -rf" — the kernel cannot see
        // inside a `sh{}` block's effects at all, so it can never mistake
        // this for shoal's own trash-based delete.
        let opaque = call(
            &mut client,
            &mut reader,
            4,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan"}),
        )
        .result
        .unwrap();
        assert_eq!(opaque["reversibility"], "irreversible");
        let opaque_rm = call(
            &mut client,
            &mut reader,
            5,
            "exec",
            json!({"src":"sh { rm -rf doomed.txt }","mode":"plan"}),
        )
        .result
        .unwrap();
        assert_eq!(
            opaque_rm["reversibility"], "irreversible",
            "an opaque external rm -rf must never be reported reversible: {opaque_rm}"
        );
        // `mv`'s source-clearing "delete" is also journaled/undoable
        // (MoveBack/RestoreBytes), so it gets the same treatment as `rm`.
        let moved = call(
            &mut client,
            &mut reader,
            6,
            "exec",
            json!({"src":"mv a.txt b.txt","mode":"plan"}),
        )
        .result
        .unwrap();
        assert_eq!(moved["reversibility"], "reversible", "{moved}");
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // Value-position multi-statement + error-still-yields-a-ref behavior; see
    // `site/content/internals/language-conformance-contract.md`.
    // -----------------------------------------------------------------------

    #[test]
    fn value_position_captures_final_expr_of_multi_statement_src() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        // Two statements; the final bare command must be *captured* (ok:false),
        // not raised — the previous single-statement-only special case raised.
        let r = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"let a = 1\nsh { exit 4 }","position":"value"}),
        );
        let value = r
            .result
            .expect("value position must not raise")
            .get("value")
            .cloned()
            .unwrap();
        assert_eq!(value["$"], "outcome");
        assert_eq!(value["ok"], false);
        assert_eq!(value["status"], 4);
        // And a binding from the first statement is visible to the last.
        let r2 = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"let x = 10\nx + 5","position":"value"}),
        );
        assert_eq!(r2.result.unwrap()["value"], json!({"$":"int","v":15}));
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn raised_error_still_yields_an_inspectable_transcript_ref() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        // A genuine raise (statement position, failed command).
        let raised = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { exit 5 }","position":"stmt"}),
        );
        let err = raised.error.expect("must raise");
        assert_eq!(err.code, RAISED);
        let data = err.data.unwrap();
        let value_ref = data["ref"]
            .as_str()
            .expect("error carries a transcript ref");
        // The agent can shoal_get that ref and read the structured error.
        let got = call(
            &mut client,
            &mut reader,
            3,
            "value.get",
            json!({"ref": value_ref}),
        );
        assert_eq!(got.result.unwrap()["value"]["$"], "error");
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // cap.request effect scoping (site/content/internals/kernel-protocol.md), complete/explain (site/content/internals/kernel-protocol.md).
    // -----------------------------------------------------------------------

    #[test]
    fn cap_request_scopes_the_grant_to_requested_effects() {
        let kernel = Kernel::new();
        // This test drives the requester and approver over ONE connection, so
        // it opts into self-acknowledgement (HR-D3); it exercises scope
        // narrowing, not the separation-of-duties gate (covered separately).
        kernel.set_allow_self_ack(true);
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let plan = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan"}),
        )
        .result
        .unwrap();
        let plan_ref = plan["plan_ref"].as_str().unwrap().to_owned();
        // Scoped to fs.write only — the plan's opaque effect isn't covered, so
        // the grant stays pending (never silently widens).
        let scoped = call(
            &mut client,
            &mut reader,
            3,
            "cap.request",
            json!({"plan_ref": plan_ref, "effects":["fs.write"]}),
        )
        .result
        .unwrap();
        assert_eq!(scoped["grant"], "approval_pending", "{scoped}");
        // Scoped to the actual effect kind — now it grants.
        let ok = call(
            &mut client,
            &mut reader,
            4,
            "cap.request",
            json!({"plan_ref": plan_ref, "effects":["opaque"]}),
        )
        .result
        .unwrap();
        assert_eq!(ok["grant"], "approved");
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// HR-D3: with self-acknowledgement OFF (the default), a plan's requester
    /// cannot approve its own plan. `cap.request` from the same principal that
    /// derived the plan is rejected with `LEASH_DENIED`, and the plan stays
    /// unapproved (`plan.apply` still fails) — approval is a genuine
    /// second-party boundary, not a rubber stamp the requester applies itself.
    #[test]
    fn cap_request_default_denies_self_approval() {
        let policy = Policy::from_toml(&format!(
            "[principal.\"{}\"]\nopaque='ask'\nauto_apply='never'\n",
            principal()
        ))
        .unwrap();
        // No `set_allow_self_ack` — self-ack defaults OFF.
        let kernel = Kernel::with_policy(policy);
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let plan = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan"}),
        )
        .result
        .unwrap();
        let plan_ref = plan["plan_ref"].as_str().unwrap().to_owned();
        let denied = call(
            &mut client,
            &mut reader,
            3,
            "cap.request",
            json!({"plan_ref": plan_ref, "effects": []}),
        );
        let err = denied
            .error
            .expect("a requester approving its own plan must be denied by default");
        assert_eq!(
            err.code, LEASH_DENIED,
            "self-approval must be LEASH_DENIED: {err:?}"
        );
        assert!(
            err.message.contains("self-approval"),
            "the denial names the reason: {}",
            err.message
        );
        // The plan is still unapproved: plan.apply refuses it.
        let apply = call(
            &mut client,
            &mut reader,
            4,
            "plan.apply",
            json!({"plan_ref": plan_ref}),
        );
        assert!(
            apply.error.is_some(),
            "a plan that was never validly approved must not apply"
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// HR-D2/HR-D3 happy path: a DISTINCT approver (a second bearer principal)
    /// may approve a requester's plan, the grant reports both identities, the
    /// approval record binds requester→approver→scope on the plan, and once the
    /// requester applies the approved plan the record names the consuming
    /// execution's journal entry. Two real bearer principals over two
    /// connections — the separation-of-duties boundary working as intended.
    #[test]
    fn cap_request_cross_principal_approval_binds_the_full_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut tokens = TokenStore::open(dir.path().join("tokens.json")).unwrap();
        let (alpha_tok, _) = tokens
            .create("agent:alpha".into(), "agent".into(), vec![], None)
            .unwrap();
        let (beta_tok, _) = tokens
            .create("agent:beta".into(), "supervisor".into(), vec![], None)
            .unwrap();
        drop(tokens);
        // Both principals must ask for opaque effects (so the plan lands
        // approval_required), and alpha's effects must not be a hard Deny (ask,
        // not deny) so approval can lift it.
        let policy = Policy::from_toml(
            "[principal.\"agent:alpha\"]\nopaque='ask'\nauto_apply='never'\n\n\
             [principal.\"agent:beta\"]\nopaque='ask'\nauto_apply='never'\n",
        )
        .unwrap();
        let kernel = Kernel::open_with_policy(dir.path(), policy).unwrap();
        // self-ack stays OFF: this is a genuine two-principal approval.

        // Requester alpha derives an approval-required plan.
        let (mut a, mut a_reader, a_thread) = spawn(&kernel);
        call(
            &mut a,
            &mut a_reader,
            1,
            "session.attach",
            json!({"token":alpha_tok,"session":"pair","client":{"kind":"agent","tty":false}}),
        );
        let planned = call(
            &mut a,
            &mut a_reader,
            2,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
        )
        .result
        .unwrap();
        assert_eq!(planned["verdict"], "approval_required", "{planned}");
        let plan_ref = planned["plan_ref"].as_str().unwrap().to_owned();

        // Approver beta (a distinct principal) approves it.
        let (mut b, mut b_reader, b_thread) = spawn(&kernel);
        call(
            &mut b,
            &mut b_reader,
            1,
            "session.attach",
            json!({"token":beta_tok,"client":{"kind":"agent","tty":false}}),
        );
        let grant = call(
            &mut b,
            &mut b_reader,
            2,
            "cap.request",
            json!({"plan_ref": plan_ref, "effects": []}),
        )
        .result
        .expect("a distinct approver may approve");
        assert_eq!(grant["grant"], "approved", "{grant}");
        assert_eq!(grant["requester"], "agent:alpha", "{grant}");
        assert_eq!(grant["approver"], "agent:beta", "{grant}");

        // The requester applies the now-approved plan; it runs.
        let applied = call(
            &mut a,
            &mut a_reader,
            3,
            "plan.apply",
            json!({"plan_ref": plan_ref}),
        )
        .result
        .expect("the requester applies its approved plan");
        assert_eq!(applied["value"]["ok"], true, "{applied}");

        // plan.get surfaces the full binding, including the consuming execution.
        let got = call(
            &mut a,
            &mut a_reader,
            4,
            "plan.get",
            json!({"plan_ref": plan_ref}),
        )
        .result
        .unwrap();
        let approval = &got["approval"];
        assert_eq!(approval["requester"], "agent:alpha", "{got}");
        assert_eq!(approval["approver"], "agent:beta", "{got}");
        assert!(
            approval["consumed_by"].is_i64(),
            "the approval names the journal entry that consumed it: {got}"
        );

        drop(a);
        drop(a_reader);
        a_thread.join().unwrap();
        drop(b);
        drop(b_reader);
        b_thread.join().unwrap();
    }

    #[test]
    fn complete_and_explain_methods() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let c = call(
            &mut client,
            &mut reader,
            2,
            "complete",
            json!({"src":"le","cursor":2}),
        )
        .result
        .unwrap();
        let candidates = c["candidates"].as_array().unwrap();
        assert!(candidates.iter().any(|v| v == "let"));
        assert!(
            candidates
                .iter()
                .all(|v| v.as_str().unwrap().starts_with("le")),
            "candidates must be filtered by the partial word"
        );
        let ex = call(
            &mut client,
            &mut reader,
            3,
            "explain",
            json!({"src":"rm gone.txt"}),
        )
        .result
        .unwrap();
        // shoal's `rm` trashes rather than deleting outright (see
        // `plan_reversibility_is_derived_from_effects`), so `explain` must
        // agree with `shoal_plan`'s answer here.
        assert_eq!(ex["reversibility"], "reversible");
        assert!(ex["ast"].is_object() || ex["ast"].is_array() || ex["ast"]["stmts"].is_array());
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn journal_until_and_effects_filters() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        call(&mut client, &mut reader, 2, "exec", json!({"src":"1 + 2"}));
        call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"rm gone.txt","position":"value"}),
        );
        // effects filter: only entries whose effect set mentions fs.delete.
        let deletes = call(
            &mut client,
            &mut reader,
            4,
            "journal.query",
            json!({"effects":["fs.delete"],"limit":50}),
        )
        .result
        .unwrap();
        let deletes = deletes.as_array().unwrap();
        assert!(!deletes.is_empty());
        assert!(
            deletes
                .iter()
                .all(|e| e["src"].as_str().unwrap().starts_with("rm")),
            "effects filter must keep only fs.delete entries: {deletes:?}"
        );
        // until in the far past matches nothing.
        let none = call(
            &mut client,
            &mut reader,
            5,
            "journal.query",
            json!({"until": 1, "limit":50}),
        )
        .result
        .unwrap();
        assert_eq!(none.as_array().unwrap().len(), 0);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn background_exec_returns_task_and_events_channel() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let bg = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { sleep 0.05 }","background":true}),
        )
        .result
        .unwrap();
        assert!(
            bg["task"].is_string(),
            "background exec returns a task ref: {bg}"
        );
        // site/content/internals/kernel-protocol.md: the events channel is `task.{bare id}` (e.g.
        // `task.7`), NOT `task.{full ref}` — the task ref itself is already
        // `task:7`, so naively prefixing it with `task.` doubles up into
        // `task.task:7`, which no `events.read`/`resources/subscribe` caller
        // can ever match against the real `task.{id}` channel.
        let task_ref = bg["task"].as_str().unwrap();
        let bare_id = task_ref.strip_prefix("task:").unwrap();
        assert_eq!(bg["events"], format!("task.{bare_id}"));
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// site/content/internals/roadmap-and-priorities.md: `task.resume` exists alongside `task.suspend`,
    /// wired the same honest way — never a silent no-op, always a clear
    /// error until a task's process handle is actually reachable here.
    #[test]
    fn task_resume_wire_method_is_honest_and_symmetric_with_suspend() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let bg = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { sleep 0.05 }","background":true}),
        )
        .result
        .unwrap();
        let task = bg["task"].clone();

        let resume = call(
            &mut client,
            &mut reader,
            3,
            "task.resume",
            json!({"task": task}),
        );
        let error = resume.error.expect("task.resume is not yet implemented");
        assert_eq!(error.code, TASK_CONTROL_UNAVAILABLE);
        assert!(
            error.message.contains("resume"),
            "message: {}",
            error.message
        );

        // Same shape as `task.suspend` for the same task.
        let suspend = call(
            &mut client,
            &mut reader,
            4,
            "task.suspend",
            json!({"task": task}),
        );
        assert_eq!(suspend.error.unwrap().code, TASK_CONTROL_UNAVAILABLE);

        // An unknown task ref is rejected before the honest-stub error, for
        // both methods.
        let unknown = json!({"task": "task:999999"});
        assert_eq!(
            call(&mut client, &mut reader, 5, "task.resume", unknown.clone())
                .error
                .unwrap()
                .code,
            UNKNOWN_TASK
        );
        assert_eq!(
            call(&mut client, &mut reader, 6, "task.suspend", unknown)
                .error
                .unwrap()
                .code,
            UNKNOWN_TASK
        );

        call(
            &mut client,
            &mut reader,
            7,
            "task.cancel",
            json!({"task": task}),
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn timeout_converts_a_slow_run_to_a_task() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        // A 30s command with a 50ms budget must come back as a task ref, never
        // block the caller's context.
        let r = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { sleep 30 }","timeout_ms":50}),
        )
        .result
        .unwrap();
        assert!(r["task"].is_string(), "a timed-out run yields a task: {r}");
        assert_eq!(r["timed_out"], true);
        // Cancel the still-running task so its `sleep 30` child doesn't linger
        // holding the test's output pipe open.
        call(
            &mut client,
            &mut reader,
            10,
            "task.cancel",
            json!({"task": r["task"]}),
        );
        // A fast command under budget returns inline.
        let fast = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"1 + 2","timeout_ms":5000}),
        )
        .result
        .unwrap();
        assert_eq!(fast["value"], json!({"$":"int","v":3}));
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // Three agent-wire honesty fixes: outcome span (site/content/internals/roadmap-and-priorities.md #4), cap.request
    // enforced (site/content/internals/roadmap-and-priorities.md #5), ANSI stripped from render on the headless/MCP
    // path (cold-agent field test finding).
    // -----------------------------------------------------------------------

    fn bare_outcome(ok: bool, stdout: &[u8]) -> Value {
        Value::Outcome(Arc::new(shoal_value::OutcomeVal {
            status: Some(if ok { 0 } else { 1 }),
            signal: None,
            ok,
            stdout: Arc::new(stdout.to_vec()),
            stdout_ref: None,
            stderr: Arc::new(Vec::new()),
            dur_ns: 1_000,
            pid: 42,
            cmd: "echo hi".into(),
            parsed: None,
            streamed: false,
            // Genuinely spanless — mirrors a builtin-wrapped or
            // journal-reconstructed outcome, exercising the honest-omission arm.
            span: None,
        }))
    }

    /// An outcome that DOES carry a source span (as the command spawn path now
    /// stamps): `bare_outcome` plus `OutcomeVal::with_span`.
    fn spanned_outcome(ok: bool, stdout: &[u8], span: shoal_ast::Span) -> Value {
        let Value::Outcome(o) = bare_outcome(ok, stdout) else {
            unreachable!()
        };
        let mut inner = Arc::try_unwrap(o).unwrap();
        inner.span = Some(span);
        Value::Outcome(Arc::new(inner))
    }

    /// Fix 1 (site/content/internals/roadmap-and-priorities.md #4): `OutcomeVal` now carries `Option<Span>`, stamped on
    /// the command spawn path with the same span the sibling error path uses
    /// (`shoal-eval/src/command.rs`). When a span is present it must reach the
    /// wire under the `span` key with the same `{start,end}` shape `ErrorVal`'s
    /// span already uses. This pins the populated direction.
    #[test]
    fn outcome_span_reaches_the_wire_when_stamped() {
        let wire = wire_value(&spanned_outcome(true, b"hi\n", shoal_ast::Span::new(3, 9)));
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            json.get("span"),
            Some(&serde_json::json!({"start": 3, "end": 9})),
            "a stamped span must travel on the wire: {json}"
        );
    }

    /// The populated span survives the elision path too (the outer `Outcome`
    /// wrapper carries its fields through even when `.out` is elided).
    #[test]
    fn outcome_span_reaches_the_wire_through_elision_too() {
        let budget = ElideBudget::default();
        let wire = elide_wire_value(
            &spanned_outcome(true, b"hi\n", shoal_ast::Span::new(3, 9)),
            "shoal://out/1",
            &budget,
        );
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            json.get("span"),
            Some(&serde_json::json!({"start": 3, "end": 9})),
            "{json}"
        );
    }

    /// The honest-omission contract still holds when the span is *genuinely*
    /// absent (builtin-wrapped / journal-reconstructed outcomes): the wire
    /// OMITS the field entirely (`skip_serializing_if`), never null-but-present
    /// and never a fabricated span.
    #[test]
    fn outcome_span_is_honestly_omitted_when_absent() {
        let wire = wire_value(&bare_outcome(true, b"hi\n"));
        let json = serde_json::to_value(&wire).unwrap();
        assert!(
            json.get("span").is_none(),
            "span must be honestly omitted when absent, not null-but-present: {json}"
        );
    }

    /// Same honest-omission contract holds through the elision path (the
    /// outer `Outcome` wrapper survives elision of a big `.out` unchanged).
    #[test]
    fn outcome_span_is_honestly_omitted_through_elision_too() {
        let budget = ElideBudget::default();
        let wire = elide_wire_value(&bare_outcome(true, b"hi\n"), "shoal://out/1", &budget);
        let json = serde_json::to_value(&wire).unwrap();
        assert!(json.get("span").is_none(), "{json}");
    }

    /// End-to-end: an outcome produced by a REAL command exec through the
    /// kernel carries a non-null `{start,end}` span on the wire — proof the
    /// eval-side stamp (`command.rs`) reaches the wire boundary, not just a
    /// hand-built `OutcomeVal`. `sh { echo hi }` spawns an external command, so
    /// it flows through `run_argv`'s spawn path (which stamps the span), not
    /// the builtin path (which leaves it `None`).
    #[test]
    fn real_command_exec_outcome_carries_a_span_on_the_wire() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo hi }"}),
        );
        let value = exec.result.unwrap()["value"].clone();
        let span = &value["span"];
        assert!(
            span.is_object(),
            "real command exec must carry a span object on the wire: {value}"
        );
        let start = span["start"].as_u64().expect("span.start");
        let end = span["end"].as_u64().expect("span.end");
        assert!(
            end > start,
            "span must cover the non-empty invocation source: {span}"
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// Fix 2 (site/content/internals/roadmap-and-priorities.md #5): `cap.request`'s grant response must report the
    /// SAME enforcement truth `session.attach`'s `caps_enforced` already
    /// does for this principal — never a hardcoded `false`. Mirrors
    /// `attach_enforces_only_for_a_scoped_principal_with_a_real_backend`'s
    /// scoped-principal setup so both endpoints are asked about the same
    /// principal and must agree.
    #[test]
    fn cap_request_reports_the_same_enforcement_truth_attach_does() {
        let who = principal();
        let policy = Policy::from_toml(&format!(
            "[principal.\"{who}\"]\nopaque='allow'\nauto_apply='in-grant'\n\n\
             [principal.\"{who}\".fs]\nread=[\"/usr/**\"]\n"
        ))
        .unwrap();
        let kernel = Kernel::with_policy(policy);
        // One-connection request→approve: opt into self-ack (HR-D3). This test
        // is about the enforcement-truth field, not the separation gate.
        kernel.set_allow_self_ack(true);
        let (mut client, mut reader, thread) = spawn(&kernel);
        let attach_result = attach(&mut client, &mut reader).result.unwrap();
        let status = EnforcementStatus::detect();
        let backend_present = matches!(
            status.available_tier,
            EnforcementTier::A | EnforcementTier::C
        );
        assert_eq!(attach_result["caps_enforced"], backend_present);
        let planned = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan"}),
        )
        .result
        .unwrap();
        let plan_ref = planned["plan_ref"].as_str().unwrap().to_owned();
        let grant = call(
            &mut client,
            &mut reader,
            3,
            "cap.request",
            json!({"plan_ref": plan_ref, "effects": []}),
        )
        .result
        .unwrap();
        assert_eq!(grant["grant"], "approved", "{grant}");
        assert_eq!(
            grant["enforced"], backend_present,
            "cap.request must report the SAME enforcement truth attach did: {grant}"
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// Baseline: the default-permissive human principal gets `enforced:false`
    /// from BOTH endpoints — a mismatch in this direction would be just as
    /// dishonest as under-reporting a real backend.
    #[test]
    fn cap_request_reports_false_for_the_default_permissive_principal() {
        let kernel = Kernel::new();
        // Single-connection self-approval to reach the grant response under
        // test: opt into self-ack (HR-D3).
        kernel.set_allow_self_ack(true);
        let (mut client, mut reader, thread) = spawn(&kernel);
        let attach_result = attach(&mut client, &mut reader).result.unwrap();
        assert_eq!(attach_result["caps_enforced"], false);
        let planned = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan"}),
        )
        .result
        .unwrap();
        let plan_ref = planned["plan_ref"].as_str().unwrap().to_owned();
        let grant = call(
            &mut client,
            &mut reader,
            3,
            "cap.request",
            json!({"plan_ref": plan_ref, "effects": []}),
        )
        .result
        .unwrap();
        assert_eq!(grant["grant"], "approved", "{grant}");
        assert_eq!(grant["enforced"], false, "{grant}");
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    // -- Fix 3: ANSI stripped from render on the headless/MCP wire ---------

    #[test]
    fn strip_ansi_removes_sgr_color_codes() {
        assert_eq!(strip_ansi("\x1b[32mhi\x1b[0m"), "hi");
    }

    #[test]
    fn strip_ansi_removes_non_sgr_csi_sequences_too() {
        // Cursor-movement/erase CSI (final bytes `H`/`K`), not just SGR
        // color (`m`) — the stripper covers the general CSI grammar.
        assert_eq!(strip_ansi("\x1b[2K\x1b[1;1Hhello"), "hello");
    }

    #[test]
    fn strip_ansi_is_a_no_op_on_plain_text() {
        assert_eq!(
            strip_ansi("plain text, no escapes here"),
            "plain text, no escapes here"
        );
    }

    /// Fix 3: on the headless/MCP path (the attaching client did not declare
    /// a real tty — what `shoal-mcp` and every shipped client attach with
    /// today), the kernel strips ANSI from the human-facing `render` string
    /// before it reaches the wire; a genuine interactive (`tty:true`) client
    /// keeps the color. The structured `value` field is untouched either
    /// way (not asserted on here — it never carried render-layer ANSI to
    /// begin with).
    #[test]
    fn headless_client_gets_ansi_stripped_render_but_a_tty_client_keeps_color() {
        // Sanity first: render_block genuinely emits ANSI for a table (the
        // bold header / dim separator are unconditional, regardless of cell
        // content) — otherwise this test would vacuously pass no matter
        // what the fix does.
        let mut row = shoal_value::Record::new();
        row.insert("n".to_string(), Value::Int(1));
        let raw = shoal_value::render::render_block(&Value::Table(vec![row]), 80);
        assert!(
            raw.contains('\u{1b}'),
            "sanity: render_block must emit ANSI for a table: {raw:?}"
        );

        // Headless (tty:false): the kernel strips it.
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
        )
        .result
        .unwrap();
        let render = exec["render"].as_str().unwrap();
        assert!(
            !render.contains('\u{1b}'),
            "headless render must be ANSI-free: {render:?}"
        );
        assert!(
            render.contains('1'),
            "content must still be present: {render:?}"
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();

        // A genuine interactive client (tty:true) keeps its color — the fix
        // must not blanket-strip regardless of what the client declared.
        let kernel2 = Kernel::new();
        let (mut client2, mut reader2, thread2) = spawn(&kernel2);
        call(
            &mut client2,
            &mut reader2,
            1,
            "session.attach",
            json!({"client":{"kind":"human","tty":true}}),
        );
        let exec2 = call(
            &mut client2,
            &mut reader2,
            2,
            "exec",
            json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
        )
        .result
        .unwrap();
        let render2 = exec2["render"].as_str().unwrap();
        assert!(
            render2.contains('\u{1b}'),
            "a real tty client must keep its color: {render2:?}"
        );
        drop(client2);
        drop(reader2);
        thread2.join().unwrap();
    }

    /// The same headless stripping applies to `value.get`'s `format=render`
    /// path (`handlers_value.rs`), not just `exec`'s inline render.
    #[test]
    fn headless_value_get_format_render_is_also_ansi_stripped() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
        )
        .result
        .unwrap();
        let value_ref = exec["ref"].as_str().unwrap().to_owned();
        let rendered = call(
            &mut client,
            &mut reader,
            3,
            "value.get",
            json!({"ref": value_ref, "format": "render"}),
        )
        .result
        .unwrap();
        let render = rendered["render"].as_str().unwrap();
        assert!(
            !render.contains('\u{1b}'),
            "headless format=render must be ANSI-free: {render:?}"
        );
        assert!(render.contains('1'), "content preserved: {render:?}");
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }
}
