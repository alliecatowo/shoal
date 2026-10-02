use crate::{Ref, RpcError, StreamCursorRef, WirePath, WireValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientInfo {
    pub kind: String,
    pub tty: bool,
}

/// Local authentication requested by a client that does not present a bearer
/// token. Restricted agent is the safe default for headless bridges; local
/// human is an explicit same-user trust-root opt-in.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LocalAuthMode {
    #[default]
    RestrictedAgent,
    LocalHuman,
}

/// Additive security-negotiation fields returned by hardened kernels from
/// `session.attach`. They remain a standalone wire projection so older Rust
/// callers constructing [`AttachResult`] are source-compatible while clients
/// can fail closed when an older kernel silently ignores `local_auth`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachSecurityMetadata {
    pub auth_mode: LocalAuthMode,
    pub session_isolation: String,
    pub security_epoch: u32,
}

/// Bumped when attachment authority semantics change incompatibly. Epoch 2
/// makes explicit that a bearer profile named `local-human` is not evidence
/// of human presence and carries no implicit approval/admin authority.
pub const ATTACH_SECURITY_EPOCH: u32 = 2;
pub const PRINCIPAL_SESSION_ISOLATION: &str = "principal";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AttachParams {
    pub session: Option<String>,
    pub token: Option<String>,
    pub client: ClientInfo,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachResult {
    pub session: String,
    pub principal: String,
    pub caps: Value,
    pub cwd: WirePath,
    pub env_hash: String,
    pub ast_version: u32,
    /// Whether the leash actually enforces (site/content/internals/language-conformance-contract.md tier honesty) — a client
    /// learns at attach time if the wall is real (site/content/internals/kernel-protocol.md).
    #[serde(default)]
    pub caps_enforced: bool,
    /// Spawn-time OS enforcement forecast. This is deliberately separate from
    /// `caps_enforced`: planning never means a backend is already active.
    #[serde(default)]
    pub enforcement: EnforcementPreview,
    /// The kernel's default elision thresholds, so a client knows the budget
    /// before it tightens/loosens per call.
    #[serde(default)]
    pub elide_defaults: Value,
    /// Channels this session may subscribe to / read (site/content/internals/kernel-protocol.md).
    #[serde(default)]
    pub channels: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseParams {
    pub src: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecParams {
    pub src: String,
    #[serde(default = "run_mode")]
    pub mode: String,
    #[serde(default = "stmt_position")]
    pub position: String,
    #[serde(default, rename = "async", alias = "background")]
    pub asynchronous: bool,
    /// Wall-clock cap (site/content/internals/kernel-protocol.md): when a synchronous `run` exceeds
    /// this, the kernel converts it to a background task and returns a task
    /// ref instead of blocking the caller's context.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Hard execution budget. Unlike `timeout_ms`, expiry requests task
    /// cancellation and remains visible on the task record. The kernel clamps
    /// extreme values to its server ceiling.
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    /// Per-call elision budget (site/content/internals/kernel-protocol.md). Tightens or loosens the
    /// kernel defaults; never loosens past the hard cap (64 KiB).
    #[serde(default)]
    pub elide: Option<ElideSpec>,
    /// Required with `mode: "approved"`: the stored plan this execution was
    /// approved under. `"approved"` is `plan.apply`'s re-entry, not a
    /// caller-assertable privilege — the kernel verifies the named plan is
    /// approved for the calling session/principal and carries the same
    /// source before skipping the leash verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_ref: Option<String>,
}

/// Per-call override of the elision thresholds (site/content/internals/kernel-protocol.md). Any field
/// left `None` keeps the kernel default for that dimension. `max_bytes` is
/// always clamped to the hard cap (64 KiB) — a misbehaving agent cannot ask
/// its way out of the wall.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ElideSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskParams {
    pub task: Ref,
}

/// `task.await` has a bounded connection-worker wait. The task itself keeps
/// running when the wait budget expires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAwaitParams {
    pub task: Ref,
    /// Omitted uses the kernel default, zero is a nonblocking snapshot, and
    /// oversized values are clamped to the server's hard wait ceiling.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}
/// `pty.open` (site/content/internals/kernel-protocol.md): spawn an interactive program on a real PTY
/// as a long-lived, keyed kernel session with a `vt100`-rendered screen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyOpenParams {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    /// Extra environment overrides layered onto the session's environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// `pty.read`/`pty.close` — identify a live PTY session by its `pty:{id}` ref.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyRefParams {
    pub pty_id: Ref,
}

/// `pty.send` — deliver input to a PTY. `input` accepts a raw string, an
/// object (`{"key":"Enter"}` / `{"text":"…"}` / `{"bytes":"<base64>"}`), or an
/// array mixing those, so an agent can express "type `i`, `hello`, Escape,
/// `:wq`, Enter" in one call (the key-name protocol; site/content/internals/kernel-protocol.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtySendParams {
    pub pty_id: Ref,
    pub input: Value,
}

/// `pty.resize` — change a live PTY's window size (and its emulator grid).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyResizeParams {
    pub pty_id: Ref,
    pub cols: u16,
    pub rows: u16,
}

/// Operations that are useful for a task at the instant its record is read.
/// These are advisory: process-backed work can start or finish immediately
/// after the snapshot, so control calls must still handle an availability
/// error.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TaskControls {
    pub cancel: bool,
    pub suspend: bool,
    pub resume: bool,
    pub active_process_groups: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task: Ref,
    pub session: String,
    pub state: String,
    pub started_ns: i64,
    pub finished_ns: Option<i64>,
    pub result_ref: Option<Ref>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub error: Option<RpcError>,
    /// Effective hard execution budget installed for this task, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    /// True only when the deadline watchdog, rather than an explicit client
    /// cancellation, requested termination.
    #[serde(default)]
    pub deadline_exceeded: bool,
    /// Race-honest control discovery for `task.cancel/suspend/resume`.
    #[serde(default)]
    pub controls: TaskControls,
}
fn run_mode() -> String {
    "run".into()
}
fn stmt_position() -> String {
    "stmt".into()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub r#ref: Ref,
    pub value: Option<WireValue>,
    pub render: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanApplyParams {
    pub plan_ref: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapRequestParams {
    pub plan_ref: Option<String>,
    #[serde(default)]
    pub effects: Vec<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanResult {
    pub plan_ref: String,
    pub effects: Vec<Value>,
    pub reversibility: String,
    pub verdict: String,
    pub approval_pending: bool,
    /// Honest per-dimension forecast for the principal that owns this plan.
    #[serde(default)]
    pub enforcement: EnforcementPreview,
}

/// What the kernel can enforce for a principal's next external spawn. Actual
/// activation is still returned by the executor after a child is launched.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnforcementPreview {
    pub available_tier: String,
    pub activation: String,
    pub filesystem_requested: bool,
    pub filesystem_enforceable: bool,
    pub network_scope_requested: bool,
    pub network_enforceable: bool,
    pub spawn_pin_requested: bool,
    pub spawn_pin_atomic: bool,
    #[serde(default)]
    pub process_limits_requested: bool,
    #[serde(default)]
    pub process_limits_enforceable: bool,
    pub hermetic: bool,
    pub spawn_disposition: String,
    #[serde(default)]
    pub limitations: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueGetParams {
    pub r#ref: Ref,
    pub path: Option<String>,
    pub slice: Option<[usize; 2]>,
    #[serde(default)]
    pub elide: Option<ElideSpec>,
    /// Response shape (site/content/internals/kernel-protocol.md): `"json"` (default) returns the
    /// `$`-tagged wire value; `"render"` returns the human render string;
    /// `"raw"` returns a str verbatim / bytes base64 (other types error).
    #[serde(default)]
    pub format: Option<String>,
    /// Preferred display width for `format:"render"`. Servers clamp this to
    /// a defensive range; omitted requests retain the historical 80 columns.
    #[serde(default)]
    pub width: Option<usize>,
}

/// `blob.get` — retrieve one bounded byte page from an owner-scoped CAS blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobGetParams {
    pub hash: String,
    /// Byte offset in the uncompressed content. Omitted means zero.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Requested byte count. The server clamps this to
    /// [`RAW_PAGE_MAX_BYTES`]. Omitted requests one maximum-size page.
    #[serde(default)]
    pub length: Option<u64>,
}

/// `stream.pull` — pull a bounded batch from a session-owned live stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamPullParams {
    pub cursor: StreamCursorRef,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Total wall-clock wait for this batch. The kernel clamps this to one
    /// second; omitted means a non-blocking poll.
    #[serde(default)]
    pub wait_ms: Option<u64>,
    /// Hard RPC execution deadline, independent of source wait. A timed-out
    /// cursor is cancelled and detached. Omitted defaults to one second.
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    #[serde(default)]
    pub elide: Option<ElideSpec>,
}

/// `stream.close` — drop the retained pipeline and release its resources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamCloseParams {
    pub cursor: StreamCursorRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamItem {
    pub seq: u64,
    pub r#ref: Ref,
    pub value: WireValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamPullResult {
    pub cursor: StreamCursorRef,
    pub items: Vec<StreamItem>,
    pub done: bool,
    pub timed_out: bool,
    /// A source/combinator error terminates the cursor but does not discard
    /// earlier items already pulled into the same batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireValue>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JournalQueryParams {
    pub since: Option<i64>,
    /// Upper time bound (ns since epoch); entries with `ts > until` are
    /// dropped. Filtered in the kernel, above the journal store.
    pub until: Option<i64>,
    pub principal: Option<String>,
    /// Exact semantic entry kind: `statement`, `exec`, or `approval`.
    pub kind: Option<String>,
    pub head: Option<String>,
    pub ok: Option<bool>,
    /// Keep only entries whose effect set contains every listed effect kind
    /// (e.g. `["fs.write","opaque"]`). Kernel-side post-filter.
    #[serde(default)]
    pub effects: Option<Vec<String>>,
    /// Maximum rows to return. **Semantics (see kernel RPC reference):**
    /// omitted/`null` → the kernel's default page size; explicit `0` → **zero
    /// rows** (an empty page, never "unbounded"); any value is clamped down to
    /// the kernel's server-side maximum page size. The distinction between
    /// omitted and an explicit `0` is exactly why this is an `Option`: a bare
    /// `usize` whose serde default is `0` cannot tell "no limit given" apart
    /// from "give me nothing".
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `events.read` — pull the buffered tail of a channel (site/content/internals/kernel-protocol.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsReadParams {
    pub channel: String,
    /// Exclusive cursor. A bounded response reports `page.next_since` when
    /// more events remain; pass that value back to continue forward.
    #[serde(default)]
    pub since: Option<u64>,
    /// Requested forward-page length. Omitted uses the server default,
    /// explicit zero returns no events, and oversized values are clamped to
    /// the server maximum before any journal query or allocation.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `events.publish` — publish to a `user.*` channel (site/content/internals/kernel-protocol.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsPublishParams {
    pub channel: String,
    pub payload: Value,
}

/// `events.subscribe` / `events.unsubscribe` (site/content/internals/kernel-protocol.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsSubParams {
    pub channel: String,
    #[serde(default)]
    pub since: Option<u64>,
}

/// One event on a channel — `seq` is monotonic per channel (site/content/internals/kernel-protocol.md).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub channel: String,
    pub seq: u64,
    pub ts: i64,
    pub payload: Value,
}

/// `complete {src, cursor?}` — completion candidates at a cursor byte offset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteParams {
    pub src: String,
    #[serde(default)]
    pub cursor: Option<usize>,
}

/// `explain {src|ast}` — derived AST + effects + reversibility without running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainParams {
    #[serde(default)]
    pub src: Option<String>,
    #[serde(default)]
    pub ast: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalOutput {
    pub kind: String,
    pub hash: String,
    pub len: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: i64,
    /// Semantic role: `statement`, `exec`, or `approval`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Owning coarse execution id for statement rows, when recorded.
    #[serde(default)]
    pub parent_id: Option<i64>,
    pub session: String,
    pub principal: String,
    pub ts: i64,
    pub dur_ns: Option<i64>,
    pub cwd: WirePath,
    pub src: String,
    pub ast: Value,
    pub effects: Value,
    pub status: Option<i32>,
    pub ok: Option<bool>,
    pub opaque: bool,
    pub outputs: Vec<JournalOutput>,
}
