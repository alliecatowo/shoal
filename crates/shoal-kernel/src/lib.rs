//! Long-lived Unix-socket host for the shoal evaluator (site/content/internals/language-conformance-contract.md).

mod completion;
mod composition;
mod connection_io;
mod connection_worker;
mod dispatch;
mod enforcement;
mod event_payload;
mod eventbus;
mod handlers_auth;
mod handlers_exec;
mod handlers_pty;
mod handlers_session;
mod handlers_stream;
mod handlers_task;
mod handlers_value;
mod lifecycle;
mod peer;
mod plan_support;
mod server;
mod session;
mod socket_lifecycle;
mod state;
mod wire;

use completion::*;
use composition::{
    AuthorityRuntime, ConnectionAdmission, LifecycleRuntime, PersistenceRuntime, SessionRuntime,
};
use connection_io::*;
use connection_worker::{ConnectionSpawner, ThreadSpawner, failure_backoff};
use event_payload::*;
use eventbus::*;
use plan_support::*;
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
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub struct Kernel {
    runtime: SessionRuntime,
    admission: ConnectionAdmission,
    persistence: PersistenceRuntime,
    authority: AuthorityRuntime,
    lifecycle: LifecycleRuntime,
}

pub use composition::KernelBuilder;
pub use socket_lifecycle::BoundSocket;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_connections: usize,
    /// Maximum principal-private evaluator sessions retained by the kernel.
    pub max_sessions: usize,
    pub max_tasks_per_session: usize,
    pub max_ptys_per_session: usize,
    pub max_ptys_per_principal: usize,
    pub max_ptys_global: usize,
    pub max_subscriptions_per_session: usize,
    /// Cache-miss CAS verification/decompression starts allowed per exact
    /// principal/session during one rate window. Cache hits do not consume it.
    pub max_blob_decompressions_per_window: usize,
    pub blob_decompression_window_ms: u64,
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
            max_sessions: 256,
            max_tasks_per_session: 128,
            max_ptys_per_session: 32,
            max_ptys_per_principal: 64,
            max_ptys_global: 256,
            max_subscriptions_per_session: 256,
            max_blob_decompressions_per_window: 64,
            blob_decompression_window_ms: 10_000,
            frame_read_timeout_ms: 10_000,
        }
    }
}

/// Wire version of the AST node-kind vocabulary (site/content/internals/language-conformance-contract.md, site/content/internals/values-streams-execution.md). Bumped
/// from 1 to 2 when `sh_raw` was retired in favor of the general
/// `lang_block` node — a breaking rename to the AST-kind enum.
const AST_VERSION: u32 = 2;

impl Kernel {
    #[must_use]
    pub fn builder() -> KernelBuilder {
        KernelBuilder::new()
    }

    pub fn new() -> Arc<Self> {
        Self::builder().build().expect("in-memory kernel")
    }

    pub fn open(state_dir: impl AsRef<Path>) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let state_dir = state_dir.as_ref();
        Self::builder().durable(state_dir).build()
    }

    pub fn open_with_policy(
        state_dir: impl AsRef<Path>,
        policy: Policy,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let state_dir = state_dir.as_ref();
        Self::builder().durable(state_dir).policy(policy).build()
    }

    /// Open a durable kernel whose credential authority file is deliberately
    /// separate from the journal/CAS state root.
    pub fn open_with_token_store(
        state_dir: impl AsRef<Path>,
        token_store: impl AsRef<Path>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        Self::builder()
            .durable(state_dir.as_ref())
            .token_store(token_store.as_ref())
            .build()
    }

    /// Policy-bearing counterpart to [`Kernel::open_with_token_store`].
    pub fn open_with_policy_and_token_store(
        state_dir: impl AsRef<Path>,
        token_store: impl AsRef<Path>,
        policy: Policy,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        Self::builder()
            .durable(state_dir.as_ref())
            .token_store(token_store.as_ref())
            .policy(policy)
            .build()
    }

    pub fn with_policy(policy: Policy) -> Arc<Self> {
        Self::builder()
            .policy(policy)
            .build()
            .expect("in-memory kernel")
    }

    fn task(&self, task: &Ref) -> Result<Arc<TaskEntry>, RpcError> {
        self.runtime.tasks.get(task)
    }

    /// Look up a live PTY session by ref, enforcing that it belongs to the
    /// calling session (an unknown ref and another session's ref are the same
    /// opaque not-found, mirroring `task`).
    fn pty(&self, pty_id: &Ref, owner: &OwnerKey) -> Result<Arc<PtyEntry>, RpcError> {
        self.runtime.ptys.get_owned(pty_id, owner)
    }

    /// Atomically remove an owned PTY. A lookup followed by a separate remove
    /// lets two concurrent closes both operate on the same child; returning
    /// the removed Arc also ensures teardown happens after the registry guard
    /// is gone.
    fn take_pty(&self, pty_id: &Ref, owner: &OwnerKey) -> Result<Arc<PtyEntry>, RpcError> {
        self.runtime.ptys.take_owned(pty_id, owner)
    }

    fn reap_finished_tasks(&self, owner: &OwnerKey) {
        self.runtime.tasks.reap_finished(owner);
    }

    /// Detect self-exited PTYs, release their active/session leases, and bound
    /// retained final-screen records. Snapshot first so registry and per-PTY
    /// locks are never held together.
    fn reap_terminal_ptys(&self, owner: &OwnerKey) -> Result<(), RpcError> {
        self.runtime.ptys.reap_terminal(owner)
    }

    /// Permit (or forbid) a plan's requester to acknowledge its own plan via
    /// `cap.request` (HR-D3). Default is forbidden — approval must come from a
    /// distinct principal. Enable only for single-operator setups that knowingly
    /// accept self-approval; the kernel binary honors `SHOAL_ALLOW_SELF_ACK` for
    /// the same purpose.
    pub fn set_allow_self_ack(&self, allow: bool) {
        self.authority.allow_self_ack.store(allow, Ordering::SeqCst);
    }

    /// Allocate a distinct stored-plan object reference. The digest is the
    /// immutable content binding; the monotonically increasing suffix prevents
    /// a second storage of identical content from replacing the first object.
    fn allocate_plan_ref(&self, plan_hash: &str) -> String {
        self.runtime.plans.allocate_ref(plan_hash)
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
        if self.authority.fail_approval_audit.load(Ordering::SeqCst) {
            return Err(internal("injected approval audit failure"));
        }
        #[cfg(test)]
        if self.authority.panic_approval_audit.load(Ordering::SeqCst) {
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
            kind: shoal_journal::EntryKind::Approval,
            parent_id: None,
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
        self.persistence
            .journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))?
            .append_completed(&record, Some(0), true, 0)
            .map_err(internal)
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

unsafe fn libc_geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

#[cfg(test)]
mod root_structure_guard {
    /// `lib.rs` is the composition root, not the owner of subsystem records.
    /// The threshold leaves modest wiring headroom above the post-split size;
    /// crossing it requires another extraction instead of silent regrowth.
    #[test]
    fn kernel_root_stays_a_composition_root() {
        const MAX_ROOT_LINES: usize = 400;
        let root = include_str!("lib.rs");
        let production_root = root
            .split("#[cfg(test)]\nmod root_structure_guard")
            .next()
            .expect("root guard marker");
        let lines = production_root.lines().count();
        assert!(
            lines <= MAX_ROOT_LINES,
            "kernel production root grew to {lines} lines (limit {MAX_ROOT_LINES}); move subsystem state to its owning module"
        );
        for state_type in [
            "TaskEntry",
            "TaskInner",
            "SessionQuota",
            "StoredPlan",
            "PlanAuthorization",
            "ApprovalRecord",
            "PtyEntry",
            "PtyLifecycle",
        ] {
            let declaration = format!("struct {state_type}");
            let enum_declaration = format!("enum {state_type}");
            assert!(
                !root.lines().any(|line| {
                    let line = line.trim_start();
                    line.starts_with(&declaration) || line.starts_with(&enum_declaration)
                }),
                "{state_type} belongs in crates/shoal-kernel/src/state, not lib.rs"
            );
        }
    }

    /// Keep the central object small enough to understand at a glance. New
    /// state belongs in one of these lifecycle/invariant groups, not directly
    /// on `Kernel`.
    #[test]
    fn kernel_owns_only_typed_subsystem_groups() {
        let root = include_str!("lib.rs");
        let marker = "pub struct Kernel {\n";
        let body = root
            .split_once(marker)
            .expect("Kernel declaration")
            .1
            .split_once("\n}")
            .expect("Kernel declaration terminator")
            .0;
        let fields: Vec<_> = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(
            fields,
            [
                "runtime: SessionRuntime,",
                "admission: ConnectionAdmission,",
                "persistence: PersistenceRuntime,",
                "authority: AuthorityRuntime,",
                "lifecycle: LifecycleRuntime,",
            ],
            "put new kernel state in the appropriate typed ownership group"
        );
    }

    /// Every compatibility constructor must select options on the public
    /// builder; the actual `Kernel` allocation stays in `composition.rs`.
    #[test]
    fn kernel_construction_is_centralized_in_the_builder() {
        let root = include_str!("lib.rs");
        let composition = include_str!("composition.rs");
        let daemon = include_str!("main.rs");
        let production_root = root
            .split("#[cfg(test)]\nmod root_structure_guard")
            .next()
            .expect("root guard marker");
        assert!(!production_root.contains("Arc::new(Kernel {"));
        assert!(!production_root.contains("Arc::new(Self {"));
        assert_eq!(composition.matches("Arc::new(Kernel {").count(), 1);
        assert!(daemon.contains("Kernel::builder()"));
        for option in [
            ".durable(&state)",
            ".token_store(&token_store)",
            ".limits(limits)",
            ".listener_security(args.require_token, args.require_peer_uid)",
        ] {
            assert!(daemon.contains(option), "daemon builder omitted {option}");
        }
        assert!(!daemon.contains("configure_limits"));
        assert!(!daemon.contains("configure_listener_security"));

        let constructors = [
            ("pub fn new()", "pub fn open("),
            ("pub fn open(", "pub fn open_with_policy("),
            ("pub fn open_with_policy(", "pub fn open_with_token_store("),
            (
                "pub fn open_with_token_store(",
                "pub fn open_with_policy_and_token_store(",
            ),
            (
                "pub fn open_with_policy_and_token_store(",
                "pub fn with_policy(",
            ),
            ("pub fn with_policy(", "fn task("),
        ];
        for (signature, next_signature) in constructors {
            let (_, from_constructor) = root
                .split_once(signature)
                .unwrap_or_else(|| panic!("missing constructor {signature}"));
            let body = from_constructor
                .split_once(next_signature)
                .unwrap_or_else(|| panic!("missing boundary {next_signature}"))
                .0;
            assert!(
                body.contains("Self::builder()"),
                "{signature} must delegate to KernelBuilder"
            );
        }
    }

    /// Domain files are named owners, not arbitrary shards introduced only to
    /// make the root line counter pass.
    #[test]
    fn extracted_kernel_domains_own_their_complete_contracts() {
        let owners = [
            (
                "server.rs",
                include_str!("server.rs"),
                &[
                    "pub fn serve(",
                    "serve_bound_until_with_spawner",
                    "handle_stream_with_trust",
                ][..],
            ),
            (
                "plan_support.rs",
                include_str!("plan_support.rs"),
                &[
                    "derive_plan",
                    "bound_plan_hash",
                    "reversibility_from_effects",
                ][..],
            ),
            (
                "event_payload.rs",
                include_str!("event_payload.rs"),
                &[
                    "transcript_event",
                    "approval_event",
                    "journal_event",
                    "render_event",
                ][..],
            ),
            (
                "completion.rs",
                include_str!("completion.rs"),
                &["complete_at", "const WORDS"][..],
            ),
        ];
        for (name, source, contracts) in owners {
            assert!(
                source.lines().count() <= 400,
                "{name} became another god module"
            );
            for contract in contracts {
                assert!(
                    source.contains(contract),
                    "{name} lost owner contract {contract}"
                );
            }
        }
    }

    #[test]
    fn kernel_unit_tests_remain_domain_split() {
        let root = include_str!("tests/mod.rs");
        let domains = [
            ("authentication", include_str!("tests/authentication.rs")),
            (
                "authority_enforcement",
                include_str!("tests/authority_enforcement.rs"),
            ),
            (
                "connections_sessions",
                include_str!("tests/connections_sessions.rs"),
            ),
            (
                "events_persistence",
                include_str!("tests/events_persistence.rs"),
            ),
            (
                "exec_plans_tasks",
                include_str!("tests/exec_plans_tasks.rs"),
            ),
            (
                "language_outcomes",
                include_str!("tests/language_outcomes.rs"),
            ),
            (
                "resource_lifecycle",
                include_str!("tests/resource_lifecycle.rs"),
            ),
            ("values_cas", include_str!("tests/values_cas.rs")),
        ];
        assert!(
            root.lines().count() <= 30,
            "test root must contain wiring only"
        );
        for (name, source) in domains {
            assert!(
                root.contains(&format!("mod {name};")),
                "missing test domain {name}"
            );
            assert!(
                source.lines().count() <= 1_250,
                "{name} test domain exceeded the kernel-specific ceiling"
            );
        }
    }
}

#[cfg(test)]
mod tests;
