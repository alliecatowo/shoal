//! Long-lived Unix-socket host for the shoal evaluator (site/content/internals/language-conformance-contract.md).

mod dispatch;
mod eventbus;
mod handlers_exec;
mod handlers_pty;
mod handlers_session;
mod handlers_stream;
mod handlers_task;
mod handlers_value;
mod lifecycle;
mod session;
mod state;
mod wire;

use eventbus::*;
use session::*;
use state::*;
use wire::*;

use serde_json::{Value as Json, json};
use shoal_ast::{CmdArg, Expr, Program, Stmt, UnOp};
use shoal_auth::{TokenMeta, TokenStore};
use shoal_eval::{EchoMode, Evaluator, Position};
use shoal_journal::{EntryRecord, Journal, JournalQuery};
use shoal_leash::{
    Effect, EnforcementStatus, EnforcementTier, Estimates, Plan, Policy, Reversibility, Verdict,
};
use shoal_proto::error_code::*;
use shoal_proto::*;
use shoal_value::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub struct Kernel {
    sessions: SessionRegistry,
    connections: ConnectionRegistry,
    max_subscriptions_per_session: AtomicUsize,
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
    plans: PlanRegistry,
    tasks: TaskRegistry,
    /// Long-lived interactive PTY sessions (site/content/internals/kernel-protocol.md), keyed by their
    /// `pty:{id}` ref like `tasks`. Each holds a live child on a real PTY plus
    /// its `vt100` emulator; scoped to the session that opened it. Dropped (and
    /// so terminated + reaped) on `pty.close` or when the kernel is dropped.
    ptys: Arc<PtyRegistry>,
    auth: Option<Mutex<TokenStore>>,
    shutdown_requested: AtomicBool,
    started_at: Instant,
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
    #[cfg(test)]
    fail_approval_audit: AtomicBool,
    #[cfg(test)]
    panic_approval_audit: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_connections: usize,
    pub max_tasks_per_session: usize,
    pub max_ptys_per_session: usize,
    pub max_ptys_per_principal: usize,
    pub max_ptys_global: usize,
    pub max_subscriptions_per_session: usize,
    /// Deadline for an unauthenticated connection's first byte and for the
    /// remainder of any frame once its first byte arrives. Zero disables it.
    pub frame_read_timeout_ms: u64,
}

/// Server-owned trust attached to a connection before any client bytes are
/// read. A wire request can never select or upgrade this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionTrust {
    /// A connection accepted from the named filesystem socket.
    Public,
    /// One anonymous socket endpoint inherited directly from a parent Shoal
    /// process. Possession is established by process inheritance, not a path.
    EmbeddedHuman,
}

impl ConnectionTrust {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::EmbeddedHuman => "embedded-human",
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_tasks_per_session: 128,
            max_ptys_per_session: 32,
            max_ptys_per_principal: 64,
            max_ptys_global: 256,
            max_subscriptions_per_session: 256,
            frame_read_timeout_ms: 10_000,
        }
    }
}

/// Wire version of the AST node-kind vocabulary (site/content/internals/language-conformance-contract.md, site/content/internals/values-streams-execution.md). Bumped
/// from 1 to 2 when `sh_raw` was retired in favor of the general
/// `lang_block` node — a breaking rename to the AST-kind enum.
const AST_VERSION: u32 = 2;

struct TaskEntry {
    task: Ref,
    owner: OwnerKey,
    session_id: String,
    /// Keeps the evaluator session live only while the worker can still use
    /// it. Terminal task records retain immutable identity/results without
    /// pinning an otherwise-idle session forever.
    session_lease: Mutex<Option<Arc<Session>>>,
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
    exit_code: Option<i32>,
    error: Option<RpcError>,
    active_slot: Option<QuotaPermit>,
}

impl TaskEntry {
    fn release_session_lease(&self) {
        let mut lease = match self.session_lease.lock() {
            Ok(lease) => lease,
            Err(poisoned) => poisoned.into_inner(),
        };
        lease.take();
        self.session_lease.clear_poison();
    }

    fn invariant_error(&self) -> RpcError {
        RpcError {
            code: INTERNAL_ERROR,
            message: "task state was reconstructed after an internal failure".into(),
            data: Some(json!({"task": self.task, "task_reconstructed": true})),
        }
    }

    fn reconstruct_locked(
        &self,
        mut inner: std::sync::MutexGuard<'_, TaskInner>,
        message: &'static str,
    ) -> Option<QuotaPermit> {
        inner.state = "failed";
        inner.finished_ns = Some(now_ns());
        inner.result_ref = None;
        inner.exit_code = None;
        inner.error = Some(RpcError {
            code: INTERNAL_ERROR,
            message: message.into(),
            data: Some(json!({"task": self.task, "task_reconstructed": true})),
        });
        inner.active_slot.take()
    }

    fn finish_reconstruction(&self, active_slot: Option<QuotaPermit>) {
        drop(active_slot);
        self.release_session_lease();
        self.done.notify_all();
    }

    fn lock_inner(&self) -> Result<std::sync::MutexGuard<'_, TaskInner>, RpcError> {
        match self.inner.lock() {
            Ok(inner) => Ok(inner),
            Err(poisoned) => {
                let inner = poisoned.into_inner();
                let active_slot = self.reconstruct_locked(inner, "task state mutex was poisoned");
                self.inner.clear_poison();
                self.finish_reconstruction(active_slot);
                Err(self.invariant_error())
            }
        }
    }

    fn repair_wait_poison(
        &self,
        poisoned: std::sync::PoisonError<std::sync::MutexGuard<'_, TaskInner>>,
    ) -> RpcError {
        let inner = poisoned.into_inner();
        let active_slot = self.reconstruct_locked(inner, "task waiter state was poisoned");
        self.inner.clear_poison();
        self.finish_reconstruction(active_slot);
        self.invariant_error()
    }

    fn repair_timeout_wait_poison(
        &self,
        poisoned: std::sync::PoisonError<(
            std::sync::MutexGuard<'_, TaskInner>,
            std::sync::WaitTimeoutResult,
        )>,
    ) -> RpcError {
        let (inner, _) = poisoned.into_inner();
        let active_slot = self.reconstruct_locked(inner, "task waiter state was poisoned");
        self.inner.clear_poison();
        self.finish_reconstruction(active_slot);
        self.invariant_error()
    }

    /// Restore the complete terminal-task invariant after a worker panic.
    /// Unlike evaluator state, TaskInner is a small host-owned record whose
    /// safe failure state can be reconstructed in full: terminal timestamp,
    /// no result, one explicit error, and no held active quota permit.
    fn fail_worker_panic(&self) {
        let (active_slot, notify) = {
            let inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(poisoned) => poisoned.into_inner(),
            };
            if inner.finished_ns.is_some() {
                let mut inner = inner;
                let active_slot = inner.active_slot.take();
                self.inner.clear_poison();
                (active_slot, false)
            } else {
                let active_slot = self.reconstruct_locked(inner, "task worker panicked");
                self.inner.clear_poison();
                (active_slot, true)
            }
        };
        drop(active_slot);
        self.release_session_lease();
        if notify {
            self.done.notify_all();
        }
    }
}

/// Ensures an unwind anywhere in task dispatch/completion cannot strand a
/// running task, its waiters, or its quota permit. Disarmed only after the
/// ordinary terminal transition and notifications have completed.
struct TaskWorkerGuard {
    task: Arc<TaskEntry>,
    kernel: Arc<Kernel>,
    channel: String,
    armed: bool,
}

impl TaskWorkerGuard {
    fn new(task: Arc<TaskEntry>, kernel: Arc<Kernel>, channel: String) -> Self {
        Self {
            task,
            kernel,
            channel,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TaskWorkerGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // A guard destructor runs during unwinding; never allow a secondary
        // failure in best-effort event publication/reaping to become a double
        // panic that aborts the process.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.task.fail_worker_panic();
            self.kernel.events.publish(
                &self.task.owner,
                &self.channel,
                json!({
                    "$": "record",
                    "v": {
                        "state": {"$":"str", "v":"failed"},
                        "ref": Json::Null,
                    }
                }),
            );
            self.kernel.reap_finished_tasks(&self.task.owner);
        }));
    }
}

/// A registered interactive PTY session (site/content/internals/kernel-protocol.md). The live
/// [`shoal_exec::PtySession`] (child + PTY master + `vt100` emulator) sits
/// behind a `Mutex` so `pty.send`/`pty.read`/`pty.resize`/`pty.close` from
/// different connections serialize on it. `session_id`/`principal` scope it to
/// its opener the same way [`TaskEntry`] scopes a task.
struct PtyEntry {
    owner: OwnerKey,
    cmd: String,
    session: Mutex<shoal_exec::PtySession>,
    lifecycle: Mutex<PtyLifecycle>,
}

struct PtyLifecycle {
    /// These leases exist only while the child is alive. A bounded terminal
    /// screen record must not consume active quota or pin evaluator state.
    session_lease: Option<Arc<Session>>,
    active_slot: Option<PtyPermit>,
    terminal_since: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
enum PtyEntryInvariant {
    Lifecycle,
}

impl PtyEntry {
    fn mark_terminal(&self) -> Result<(), PtyEntryInvariant> {
        let (active_slot, session_lease) = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .map_err(|_| PtyEntryInvariant::Lifecycle)?;
            if lifecycle.terminal_since.is_some() {
                return Ok(());
            }
            lifecycle.terminal_since = Some(Instant::now());
            (lifecycle.active_slot.take(), lifecycle.session_lease.take())
        };
        drop((active_slot, session_lease));
        Ok(())
    }

    fn terminal_since(&self) -> Result<Option<Instant>, PtyEntryInvariant> {
        self.lifecycle
            .lock()
            .map(|lifecycle| lifecycle.terminal_since)
            .map_err(|_| PtyEntryInvariant::Lifecycle)
    }
}

#[derive(Default)]
struct SessionQuota {
    counts: Mutex<HashMap<OwnerKey, usize>>,
    quarantined: AtomicBool,
}

struct QuotaPermit {
    quota: Arc<SessionQuota>,
    owner: OwnerKey,
}

impl SessionQuota {
    fn reserve(
        self: &Arc<Self>,
        owner: &OwnerKey,
        max: usize,
        limit: &'static str,
        noun: &'static str,
    ) -> Result<QuotaPermit, RpcError> {
        if self.quarantined.load(Ordering::Acquire) || self.counts.is_poisoned() {
            self.quarantined.store(true, Ordering::Release);
            return Err(task_quota_unavailable());
        }
        let mut counts = match self.counts.lock() {
            Ok(counts) => counts,
            Err(poisoned) => {
                drop(poisoned);
                self.quarantined.store(true, Ordering::Release);
                return Err(task_quota_unavailable());
            }
        };
        let current = counts.entry(owner.clone()).or_default();
        if *current >= max {
            return Err(RpcError {
                code: QUOTA_EXCEEDED,
                message: format!("session has reached the {max}-{noun} limit"),
                data: Some(json!({"limit": limit, "max": max})),
            });
        }
        *current += 1;
        Ok(QuotaPermit {
            quota: self.clone(),
            owner: owner.clone(),
        })
    }
}

impl Drop for QuotaPermit {
    fn drop(&mut self) {
        if self.quota.quarantined.load(Ordering::Acquire) {
            return;
        }
        let mut counts = match self.quota.counts.lock() {
            Ok(counts) => counts,
            Err(poisoned) => {
                drop(poisoned);
                self.quota.quarantined.store(true, Ordering::Release);
                return;
            }
        };
        if let Some(current) = counts.get_mut(&self.owner) {
            *current = current.saturating_sub(1);
            if *current == 0 {
                counts.remove(&self.owner);
            }
        }
    }
}

fn task_quota_unavailable() -> RpcError {
    RpcError {
        code: INTERNAL_ERROR,
        message: "task quota is quarantined; restart the kernel".into(),
        data: Some(json!({
            "subsystem": "task_quota",
            "quarantined": true,
            "restart_required": true,
        })),
    }
}

const PLAN_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
const GRANT_RESERVATION_TTL: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_STORED_PLANS_PER_OWNER: usize = 256;
const MAX_PLAN_SOURCE_BYTES_PER_OWNER: usize = 64 * 1024 * 1024;

struct StoredPlan {
    src: String,
    session: String,
    /// The plan owner / **requester** — the principal that derived this plan
    /// (`exec {mode:"plan"}`). Distinct from an `ApprovalRecord::approver`.
    principal: String,
    /// Full (untruncated) BLAKE3 binding of source, canonical AST, derived plan,
    /// session, and requester. Unlike `plan_ref`, this is content identity.
    plan_hash: String,
    /// Full source digest kept separately so approval/audit projections can
    /// state exactly which source was authorized without copying source text.
    source_hash: String,
    plan: Plan,
    authorization: PlanAuthorization,
    created_at: Instant,
}

fn plan_expired(plan: &StoredPlan) -> bool {
    // An in-flight transition owns this plan until its durable side effect is
    // resolved. Purging it here could leave an approval audit or execution
    // claim with no state to commit or roll back into.
    !matches!(
        plan.authorization,
        PlanAuthorization::Granting { .. } | PlanAuthorization::Claimed(_)
    ) && plan.created_at.elapsed() > PLAN_TTL
}

impl StoredPlan {
    fn recover_stale_grant(&mut self) {
        let restore_policy_allowed = match &self.authorization {
            PlanAuthorization::Granting {
                restore_policy_allowed,
                started_at,
                lease,
                ..
            } if lease.upgrade().is_none() && started_at.elapsed() >= GRANT_RESERVATION_TTL => {
                Some(*restore_policy_allowed)
            }
            _ => None,
        };
        if let Some(restore_policy_allowed) = restore_policy_allowed {
            self.authorization = if restore_policy_allowed {
                PlanAuthorization::PolicyAllowed
            } else {
                PlanAuthorization::Pending
            };
        }
    }
}

/// Authorization is a one-way state machine. An explicit approval is a
/// single-use capability: claiming it excludes concurrent/replayed applies,
/// and a completed execution can never be returned to the approved state.
#[derive(Clone)]
enum PlanAuthorization {
    PolicyAllowed,
    Pending,
    Denied,
    /// A validated approval whose durable grant audit is being appended
    /// outside the plan-registry transaction. No apply or second grant may
    /// pass this state.
    Granting {
        record: ApprovalRecord,
        restore_policy_allowed: bool,
        started_at: Instant,
        lease: std::sync::Weak<()>,
    },
    Approved(ApprovalRecord),
    Claimed(ApprovalRecord),
    Consumed(ApprovalRecord),
}

impl PlanAuthorization {
    fn is_approved(&self) -> bool {
        matches!(
            self,
            Self::PolicyAllowed | Self::Approved(_) | Self::Claimed(_) | Self::Consumed(_)
        )
    }

    fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    fn approval(&self) -> Option<&ApprovalRecord> {
        match self {
            Self::Approved(record) | Self::Claimed(record) | Self::Consumed(record) => Some(record),
            Self::PolicyAllowed | Self::Pending | Self::Denied | Self::Granting { .. } => None,
        }
    }
}

/// The auditable record binding an approval to its requester, plan, approver,
/// scope, and consuming execution (HR-D2). Mirrored into the journal as an
/// audit entry at approval time (`record_approval_audit`) and surfaced on
/// `plan.get` so the whole chain is inspectable, never an unattributed bit.
#[derive(Clone, PartialEq, Eq)]
struct ApprovalRecord {
    /// The plan owner whose effects were approved.
    requester: String,
    /// The authenticated, authorized principal that approved (the
    /// `cap.request` caller; distinct unless self-ack was explicitly enabled).
    approver: String,
    /// The source-anchored plan ref/hash this approval is bound to.
    plan_ref: String,
    /// Immutable full plan/content digest copied from [`StoredPlan`].
    plan_hash: String,
    /// Immutable full source digest copied from [`StoredPlan`].
    source_hash: String,
    /// Session in which the requester derived the plan.
    session: String,
    /// The effect kinds the approval was scoped to (empty ⇒ the whole plan).
    scope: Vec<String>,
    /// When the approval was granted (ns since epoch).
    approved_at_ns: i64,
    /// Completed journal row that durably records the grant itself.
    grant_audit_id: i64,
    /// The journal entry id of the execution that consumed this approval, once
    /// an approved `exec` actually ran the plan. `None` until then.
    consumed_by: Option<i64>,
}

impl Kernel {
    pub fn new() -> Arc<Self> {
        let limits = Limits::default();
        Arc::new(Self {
            sessions: SessionRegistry::new(),
            connections: ConnectionRegistry::new(
                limits.max_connections,
                limits.frame_read_timeout_ms,
            ),
            max_subscriptions_per_session: AtomicUsize::new(limits.max_subscriptions_per_session),
            journal: Mutex::new(Journal::in_memory().expect("in-memory journal")),
            state_dir: None,
            policy: permissive_policy(),
            plans: PlanRegistry::new(),
            tasks: TaskRegistry::new(limits.max_tasks_per_session),
            ptys: Arc::new(PtyRegistry::new(
                limits.max_ptys_per_session,
                limits.max_ptys_per_principal,
                limits.max_ptys_global,
            )),
            events: Arc::new(EventBus::default()),
            auth: None,
            shutdown_requested: AtomicBool::new(false),
            started_at: Instant::now(),
            allow_self_ack: AtomicBool::new(self_ack_from_env()),
            #[cfg(test)]
            fail_approval_audit: AtomicBool::new(false),
            #[cfg(test)]
            panic_approval_audit: AtomicBool::new(false),
        })
    }

    pub fn open(state_dir: impl AsRef<Path>) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let state_dir = state_dir.as_ref();
        let journal = Journal::open(state_dir)?;
        let events = EventBus::default();
        let limits = Limits::default();
        // Reopening an EXISTING on-disk store must resume its
        // `journal`/`session.transcript` seq state, not restart both at 0 —
        // see `EventBus::seed_from_journal` for why (a reconnecting agent's
        // persisted `since=N` cursor would otherwise collide with a
        // brand-new seq the freshly-restarted kernel hands out starting
        // from 0 again). A fresh, empty store is a no-op: both channels
        // correctly still start at 0.
        events.seed_from_journal(&journal);
        Ok(Arc::new(Self {
            sessions: SessionRegistry::new(),
            connections: ConnectionRegistry::new(
                limits.max_connections,
                limits.frame_read_timeout_ms,
            ),
            max_subscriptions_per_session: AtomicUsize::new(limits.max_subscriptions_per_session),
            journal: Mutex::new(journal),
            state_dir: Some(state_dir.to_path_buf()),
            policy: permissive_policy(),
            plans: PlanRegistry::new(),
            tasks: TaskRegistry::new(limits.max_tasks_per_session),
            ptys: Arc::new(PtyRegistry::new(
                limits.max_ptys_per_session,
                limits.max_ptys_per_principal,
                limits.max_ptys_global,
            )),
            events: Arc::new(events),
            auth: Some(Mutex::new(TokenStore::open(state_dir.join("tokens.json"))?)),
            shutdown_requested: AtomicBool::new(false),
            started_at: Instant::now(),
            allow_self_ack: AtomicBool::new(self_ack_from_env()),
            #[cfg(test)]
            fail_approval_audit: AtomicBool::new(false),
            #[cfg(test)]
            panic_approval_audit: AtomicBool::new(false),
        }))
    }

    pub fn open_with_policy(
        state_dir: impl AsRef<Path>,
        policy: Policy,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let state_dir = state_dir.as_ref();
        let journal = Journal::open(state_dir)?;
        let events = EventBus::default();
        let limits = Limits::default();
        // Same restart-seq-continuity fix as `Kernel::open` above.
        events.seed_from_journal(&journal);
        Ok(Arc::new(Self {
            sessions: SessionRegistry::new(),
            connections: ConnectionRegistry::new(
                limits.max_connections,
                limits.frame_read_timeout_ms,
            ),
            max_subscriptions_per_session: AtomicUsize::new(limits.max_subscriptions_per_session),
            journal: Mutex::new(journal),
            state_dir: Some(state_dir.to_path_buf()),
            policy,
            plans: PlanRegistry::new(),
            tasks: TaskRegistry::new(limits.max_tasks_per_session),
            ptys: Arc::new(PtyRegistry::new(
                limits.max_ptys_per_session,
                limits.max_ptys_per_principal,
                limits.max_ptys_global,
            )),
            events: Arc::new(events),
            auth: Some(Mutex::new(TokenStore::open(state_dir.join("tokens.json"))?)),
            shutdown_requested: AtomicBool::new(false),
            started_at: Instant::now(),
            allow_self_ack: AtomicBool::new(self_ack_from_env()),
            #[cfg(test)]
            fail_approval_audit: AtomicBool::new(false),
            #[cfg(test)]
            panic_approval_audit: AtomicBool::new(false),
        }))
    }

    pub fn with_policy(policy: Policy) -> Arc<Self> {
        let limits = Limits::default();
        Arc::new(Self {
            sessions: SessionRegistry::new(),
            connections: ConnectionRegistry::new(
                limits.max_connections,
                limits.frame_read_timeout_ms,
            ),
            max_subscriptions_per_session: AtomicUsize::new(limits.max_subscriptions_per_session),
            journal: Mutex::new(Journal::in_memory().expect("in-memory journal")),
            state_dir: None,
            policy,
            plans: PlanRegistry::new(),
            tasks: TaskRegistry::new(limits.max_tasks_per_session),
            ptys: Arc::new(PtyRegistry::new(
                limits.max_ptys_per_session,
                limits.max_ptys_per_principal,
                limits.max_ptys_global,
            )),
            events: Arc::new(EventBus::default()),
            auth: None,
            shutdown_requested: AtomicBool::new(false),
            started_at: Instant::now(),
            allow_self_ack: AtomicBool::new(self_ack_from_env()),
            #[cfg(test)]
            fail_approval_audit: AtomicBool::new(false),
            #[cfg(test)]
            panic_approval_audit: AtomicBool::new(false),
        })
    }

    pub fn configure_limits(&self, limits: Limits) {
        self.connections
            .configure(limits.max_connections, limits.frame_read_timeout_ms);
        self.tasks.configure(limits.max_tasks_per_session);
        self.ptys.configure(
            limits.max_ptys_per_session,
            limits.max_ptys_per_principal,
            limits.max_ptys_global,
        );
        self.max_subscriptions_per_session
            .store(limits.max_subscriptions_per_session, Ordering::Relaxed);
    }

    fn reserve_connection_slot(&self) -> Result<ConnectionPermit, ()> {
        self.connections.reserve()
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
        while !stop.load(Ordering::SeqCst) && !self.shutdown_requested.load(Ordering::SeqCst) {
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
                    let slot = match kernel.reserve_connection_slot() {
                        Ok(slot) => slot,
                        Err(()) => {
                            let max = kernel.connections.max();
                            let _ = reject_connection_over_quota(stream, max);
                            continue;
                        }
                    };
                    std::thread::Builder::new()
                        .name("shoal-kernel-connection".into())
                        .spawn(move || {
                            let _slot = slot;
                            let _ =
                                kernel.handle_stream_with_trust(stream, ConnectionTrust::Public);
                        })?;
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
        self.handle_stream_with_trust(stream, ConnectionTrust::Public)
    }

    /// Service one already-connected stream under server-selected trust.
    /// Public listeners must always pass [`ConnectionTrust::Public`].
    pub fn handle_stream_with_trust(
        self: &Arc<Self>,
        stream: UnixStream,
        trust: ConnectionTrust,
    ) -> io::Result<()> {
        let client = self.connections.next_client();
        let mut reader = BufReader::new(stream.try_clone()?);
        let writer: SharedWriter = Arc::new(Mutex::new(stream));
        let mut attached: Option<Attachment> = None;
        let result = (|| -> io::Result<()> {
            loop {
                let timeout_ms = self.connections.frame_read_timeout_ms();
                // Before authentication, a client must begin its first frame
                // within the deadline. Once attached, an entirely idle client
                // may remain subscribed indefinitely; after the first byte of
                // any new frame, however, the same deadline bounds completion.
                let bearer_idle_recheck = attached
                    .as_ref()
                    .is_some_and(|attachment| attachment.bearer.is_some());
                let wait_timeout = if attached.is_none() && timeout_ms != 0 {
                    Some(timeout_ms)
                } else if bearer_idle_recheck {
                    // Disabling the frame deadline must not disable bearer
                    // revocation. Otherwise use the configured deadline as
                    // the maximum stale-authority window as well.
                    Some(if timeout_ms == 0 { 10_000 } else { timeout_ms })
                } else {
                    None
                };
                set_read_deadline(reader.get_ref(), wait_timeout)?;
                match reader.fill_buf() {
                    Ok([]) => break,
                    Ok(_) => {}
                    Err(error) if bearer_idle_recheck && is_read_timeout(&error) => {
                        let validity = attached
                            .as_ref()
                            .expect("bearer recheck requires an attachment");
                        if let Err(error) = self.ensure_attachment_current(validity) {
                            self.events.remove_conn(client);
                            attached = None;
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                error.message,
                            ));
                        }
                        continue;
                    }
                    Err(error) => return Err(error),
                }
                set_read_deadline(reader.get_ref(), (timeout_ms != 0).then_some(timeout_ms))?;
                let Some(request) = read_frame(&mut reader)? else {
                    break;
                };
                let id = request.id.clone();
                let response = if request.jsonrpc != JSONRPC {
                    Response::err(id, INVALID_REQUEST, "invalid JSON-RPC version", None)
                } else {
                    self.dispatch(request, client, &mut attached, Some(&writer), trust)
                };
                // A poisoned writer may contain a partially-written JSON
                // frame. Close this connection rather than recovering the
                // guard and corrupting framing for subsequent responses.
                let mut writer = writer
                    .lock()
                    .map_err(|_| io::Error::other("connection writer poisoned"))?;
                write_frame(&mut *writer, &response)?;
            }
            Ok(())
        })();
        // On disconnect, drop this connection's subscriptions so publish never
        // writes to a dead fd.
        self.events.remove_conn(client);
        result
    }

    fn task(&self, task: &Ref) -> Result<Arc<TaskEntry>, RpcError> {
        self.tasks.get(task)
    }

    /// Look up a live PTY session by ref, enforcing that it belongs to the
    /// calling session (an unknown ref and another session's ref are the same
    /// opaque not-found, mirroring `task`).
    fn pty(&self, pty_id: &Ref, owner: &OwnerKey) -> Result<Arc<PtyEntry>, RpcError> {
        self.ptys.get_owned(pty_id, owner)
    }

    /// Atomically remove an owned PTY. A lookup followed by a separate remove
    /// lets two concurrent closes both operate on the same child; returning
    /// the removed Arc also ensures teardown happens after the registry guard
    /// is gone.
    fn take_pty(&self, pty_id: &Ref, owner: &OwnerKey) -> Result<Arc<PtyEntry>, RpcError> {
        self.ptys.take_owned(pty_id, owner)
    }

    fn reap_finished_tasks(&self, owner: &OwnerKey) {
        self.tasks.reap_finished(owner);
    }

    /// Detect self-exited PTYs, release their active/session leases, and bound
    /// retained final-screen records. Snapshot first so registry and per-PTY
    /// locks are never held together.
    fn reap_terminal_ptys(&self, owner: &OwnerKey) -> Result<(), RpcError> {
        self.ptys.reap_terminal(owner)
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

    /// Allocate a distinct stored-plan object reference. The digest is the
    /// immutable content binding; the monotonically increasing suffix prevents
    /// a second storage of identical content from replacing the first object.
    fn allocate_plan_ref(&self, plan_hash: &str) -> String {
        self.plans.allocate_ref(plan_hash)
    }

    /// Append a completed journal audit entry for an approval decision (HR-D2), so the
    /// requester→plan→approver→scope binding is durably queryable via
    /// `journal.query`, not just live in the plan map. This is fail-closed: an
    /// approval that could not be durably audited is not granted.
    fn record_approval_audit(
        &self,
        approval: &ApprovalRecord,
        effect_kinds: &[String],
        session: &str,
    ) -> Result<i64, RpcError> {
        #[cfg(test)]
        if self.fail_approval_audit.load(Ordering::SeqCst) {
            return Err(internal("injected approval audit failure"));
        }
        #[cfg(test)]
        if self.panic_approval_audit.load(Ordering::SeqCst) {
            panic!("injected approval audit panic");
        }
        let effects_json = serde_json::to_string(&json!([{
            "kind": "approval",
            "plan_ref": approval.plan_ref,
            "plan_hash": approval.plan_hash,
            "source_hash": approval.source_hash,
            "session": approval.session,
            "requester": approval.requester,
            "approver": approval.approver,
            "scope": approval.scope,
            "effects": effect_kinds,
        }]))
        .unwrap_or_else(|_| "[]".into());
        let record = EntryRecord {
            session: session.to_string(),
            // The grant mutates the requester's plan and is consumed by the
            // requester's later execution, so store it in that exact owner's
            // journal partition. The embedded effect still names the distinct
            // approver for attribution and separation-of-duties auditing.
            principal: approval.requester.clone(),
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
        self.journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))?
            .append_completed(&record, Some(0), true, 0)
            .map_err(internal)
    }
}

fn set_read_deadline(stream: &UnixStream, timeout_ms: Option<u64>) -> io::Result<()> {
    stream.set_read_timeout(timeout_ms.map(std::time::Duration::from_millis))
}

fn is_read_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

fn task_record(task: &Arc<TaskEntry>) -> Result<TaskRecord, RpcError> {
    let inner = match task.lock_inner() {
        Ok(inner) => inner,
        // `lock_inner` fully reconstructs and unpoisons TaskInner before
        // returning this first error. Reads retry once so callers observe the
        // durable terminal failure record; mutation paths retain the error.
        Err(_) => task.lock_inner()?,
    };
    Ok(task_record_locked(task, &inner))
}
fn task_record_locked(task: &TaskEntry, inner: &TaskInner) -> TaskRecord {
    TaskRecord {
        task: task.task.clone(),
        session: task.session_id.clone(),
        state: inner.state.into(),
        started_ns: task.started_ns,
        finished_ns: inner.finished_ns,
        result_ref: inner.result_ref.clone(),
        exit_code: inner.exit_code,
        error: inner.error.clone(),
    }
}

struct BoundSocket(std::path::PathBuf);
impl Drop for BoundSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn reject_connection_over_quota(mut stream: UnixStream, max_connections: usize) -> io::Result<()> {
    stream.set_write_timeout(Some(std::time::Duration::from_millis(100)))?;
    write_frame(
        &mut stream,
        &Response::err(
            Json::Null,
            QUOTA_EXCEEDED,
            format!("kernel connection limit ({max_connections}) reached"),
            Some(json!({"limit":"connections", "max":max_connections})),
        ),
    )
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
/// `cap.request`) is permitted by process configuration (HR-D3). Only explicit
/// boolean true spellings enable it; notably `0`, `false`, and an empty value
/// remain false. Read once per kernel at construction; `set_allow_self_ack`
/// can override it at runtime.
fn self_ack_from_env() -> bool {
    parse_env_bool(std::env::var_os("SHOAL_ALLOW_SELF_ACK").as_deref())
}

fn parse_env_bool(value: Option<&std::ffi::OsStr>) -> bool {
    value
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
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
    format!("plan:{}", hasher.finalize().to_hex())
}

fn source_hash(src: &str) -> String {
    blake3::hash(src.as_bytes()).to_hex().to_string()
}

/// Full immutable approval binding. This deliberately excludes `plan_ref`
/// because that contains the per-kernel object id; all semantic inputs are
/// included explicitly and domain-separated.
fn bound_plan_hash(
    src: &str,
    ast_json: &str,
    plan: &Plan,
    session: &str,
    requester: &str,
) -> String {
    let canonical = serde_json::to_vec(&(
        src,
        ast_json,
        &plan.effects,
        plan.reversibility,
        &plan.estimates,
        session,
        requester,
    ))
    .expect("plan binding is serializable");
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"shoal.kernel.plan-binding.v1\0");
    hasher.update(&canonical);
    hasher.finalize().to_hex().to_string()
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
            evaluator.set_it(value.clone());
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
            "plan_hash": a.plan_hash,
            "source_hash": a.source_hash,
            "session": a.session,
            "scope": a.scope,
            "approved_at": a.approved_at_ns,
            "grant_audit_id": a.grant_audit_id,
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
mod tests;
