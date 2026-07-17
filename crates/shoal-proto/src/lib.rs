//! Shoal's newline-framed JSON-RPC 2.0 wire contract (site/content/internals/language-conformance-contract.md).

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

pub const JSONRPC: &str = "2.0";
pub type RequestId = Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// The complete `RpcError.code` taxonomy (site/content/internals/language-conformance-contract.md, site/content/internals/kernel-protocol.md's
/// "error codes" table). Every `RpcError` built anywhere in `shoal-kernel`
/// is constructed with one of these named constants rather than an inline
/// `-32XXX` literal — this module is the single place the mapping from
/// number to meaning lives, so it can't drift silently between call sites.
///
/// Two families, per JSON-RPC 2.0's own reserved ranges:
/// - `-32700..=-32600`: the spec's own pre-defined codes (documented here for
///   completeness; shoal-kernel itself only emits [`INVALID_REQUEST`],
///   [`METHOD_NOT_FOUND`], [`INVALID_PARAMS`], and [`INTERNAL_ERROR`] —
///   [`RPC_PARSE_ERROR`] is a framing-level error only the `shoal-mcp` stdio
///   bridge raises today; a `shoal-kernel` frame that fails to parse as JSON
///   never becomes an `RpcError` at all, it just ends the connection).
/// - `-32000..=-32099`: JSON-RPC's "implementation-defined server error"
///   band, which is where every shoal-kernel-specific code below lives.
///
/// A handful of these numbers are **overloaded** — reused across call sites
/// whose meanings are related but not identical (most notably
/// [`LEASH_DENIED`]). Each doc comment below says so explicitly; this is a
/// pure refactor of pre-existing behavior, not a new design decision, so the
/// overloads are preserved rather than split into new codes (wire codes must
/// stay byte-identical — see the crate's `wire_codes_match_taxonomy` test).
pub mod error_code {
    // --- JSON-RPC 2.0 spec-reserved codes ---

    /// Invalid JSON was received (frame-level parse failure). Not raised by
    /// `shoal-kernel` (a bad frame just ends the connection via `io::Error`
    /// before any `RpcError` exists); `shoal-mcp`'s stdio bridge uses this
    /// code when *its* line-framed JSON fails to parse.
    pub const RPC_PARSE_ERROR: i32 = -32700;
    /// The request was not a valid JSON-RPC 2.0 envelope (today: the wrong
    /// `jsonrpc` version string).
    pub const INVALID_REQUEST: i32 = -32600;
    /// The requested method name has no handler in `Kernel::dispatch`.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// The method's params failed to decode into the expected shape, or an
    /// enum-like field (`exec`'s `mode`, `value.get`'s `format`) held a value
    /// outside its accepted set, or a param violated a scoping rule (e.g.
    /// `events.publish` off a non-`user.*` channel).
    pub const INVALID_PARAMS: i32 = -32602;
    /// An unexpected local failure the caller couldn't have prevented
    /// (serialization, journal I/O). Also (overloaded) used for
    /// `events.subscribe` without a live connection to subscribe on, which is
    /// a caller/environment condition rather than a genuine internal bug.
    pub const INTERNAL_ERROR: i32 = -32603;

    // --- shoal-kernel server-error range (-32000..=-32099) ---

    /// No session is attached on this connection yet. Every handler but
    /// `session.attach`, `parse`, and `complete` requires one. (`cap.request`
    /// and `journal.query`, once exempt, now require attachment too — HR-D1/D4.)
    pub const NOT_ATTACHED: i32 = -32000;
    /// The submitted shoal *source* failed to parse (`shoal_syntax::parse`).
    /// Distinct from [`RPC_PARSE_ERROR`]: this is a language-level parse
    /// error carried as a normal RPC error, not a wire-framing failure.
    pub const PARSE_ERROR: i32 = -32001;
    /// Evaluating the parsed source raised a shoal-language error. The
    /// raised `ErrorVal` is still addressable afterward via the `out[n]`
    /// transcript ref carried in the error's `data`.
    pub const RAISED: i32 = -32002;
    /// The `ref`/`hash` named by `value.get` or `blob.get` doesn't name
    /// anything this session's transcript (or the journal/CAS) knows about,
    /// or a CAS-backed bytes ref failed to resolve its stored content.
    pub const UNKNOWN_REF: i32 = -32004;
    /// A `value.get` request's `path`, `slice`, or `format` doesn't match
    /// the shape of the value it targets (bad field path, an out-of-kind
    /// slice, or a `format` the value's type doesn't support).
    pub const BAD_PATH_OR_SLICE: i32 = -32005;
    /// The leash policy forbids the requested operation. **Overloaded**
    /// across three related-but-distinct conditions (see
    /// `site/content/internals/kernel-protocol.md`'s error-codes table): a plain
    /// `Verdict::Deny` on `exec {mode:"run"}`; a `plan_ref`/task lookup
    /// (`plan.get`/`plan.apply`) that names a plan belonging to a different
    /// principal/session; and an `exec {mode:"approved"}` re-entry that
    /// fails to verify against a stored, approved plan for this
    /// session/principal.
    pub const LEASH_DENIED: i32 = -32010;
    /// The leash policy requires explicit approval
    /// (`Verdict::ApprovalRequired`) before this plan/effect set may run —
    /// `plan` it, then `cap.request`, then re-`exec` with `mode:"approved"`.
    pub const APPROVAL_REQUIRED: i32 = -32011;
    /// The named `plan_ref` (`plan.get`/`plan.apply`/`cap.request`) is
    /// unknown or has expired.
    pub const UNKNOWN_PLAN: i32 = -32012;
    /// Task suspend/resume is unavailable: a kernel task is a Rust thread
    /// recursively re-entering `dispatch`, not a single tracked child
    /// process/group, so there is nothing to signal yet.
    pub const TASK_CONTROL_UNAVAILABLE: i32 = -32020;
    /// The named `task` ref is unknown, or belongs to another session.
    pub const UNKNOWN_TASK: i32 = -32021;
    /// The named `pty_id` (`pty.send`/`pty.read`/`pty.resize`/`pty.close`) is
    /// unknown, already closed, or belongs to another session.
    pub const UNKNOWN_PTY: i32 = -32022;
    /// A `pty.open` could not spawn the requested program on a PTY — the
    /// program was not resolvable, the sandbox could not be applied, or PTY/
    /// spawn plumbing failed (the underlying `io::Error` travels in the message).
    pub const PTY_SPAWN_FAILED: i32 = -32023;
    /// Bearer-token authentication failed on `session.attach`: either this
    /// kernel has no `TokenStore` configured at all (an ephemeral kernel),
    /// or the given token is missing/expired/revoked.
    pub const AUTH_FAILED: i32 = -32030;
}

impl Response {
    pub fn ok(id: RequestId, value: impl Serialize) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id,
            result: Some(serde_json::to_value(value).expect("serializable RPC result")),
            error: None,
        }
    }
    pub fn err(id: RequestId, code: i32, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

/// Maximum size of one newline-delimited JSON-RPC frame.
///
/// The limit is applied while reading, rather than after `read_line` has
/// already buffered an arbitrarily large or unterminated input.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

pub fn read_frame<R: BufRead>(reader: &mut R) -> io::Result<Option<Request>> {
    let mut line = String::new();
    let n = reader
        .by_ref()
        .take(MAX_FRAME_LEN as u64 + 1)
        .read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "JSON-RPC frame exceeds 16 MiB",
        ));
    }
    serde_json::from_str(line.trim_end())
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, frame: &T) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, frame).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct Ref(pub String);

impl Ref {
    pub fn new(kind: &str, id: impl std::fmt::Display) -> Self {
        Self(format!("{kind}:{id}"))
    }
    pub fn kind(&self) -> Option<&str> {
        self.0.split_once(':').map(|x| x.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "$", rename_all = "snake_case")]
pub enum WireValue {
    Null,
    Bool {
        v: bool,
    },
    Int {
        v: i64,
    },
    Float {
        v: f64,
    },
    Str {
        v: String,
    },
    Size {
        v: u64,
    },
    Duration {
        v: i64,
    },
    Bytes {
        v: String,
    },
    Path {
        v: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw: Option<String>,
    },
    List {
        v: Vec<WireValue>,
    },
    Record {
        v: BTreeMap<String, WireValue>,
    },
    /// Columnar per site/content/internals/language-conformance-contract.md: every row contributes to every column (missing
    /// cells encode as `null`), so `cols[c].len() == n` for every column.
    Table {
        cols: BTreeMap<String, Vec<WireValue>>,
        n: usize,
    },
    Outcome {
        status: Option<i32>,
        ok: bool,
        signal: Option<String>,
        out: Box<WireValue>,
        /// Lossy UTF-8 of stderr — not a CAS ref; large payloads are still
        /// truncated at the journal layer, this is the live wire copy.
        err: String,
        dur_ns: i64,
        pid: u32,
        cmd: String,
        /// Source span of the invocation (site/content/internals/kernel-protocol.md). `None` when the
        /// outcome carries no source anchor (e.g. a value reconstructed from
        /// the journal); omitted from the wire when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span: Option<WireSpan>,
    },
    Error {
        code: String,
        msg: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<WireSpan>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stderr: Option<String>,
    },
    #[serde(rename = "datetime")]
    DateTime {
        /// RFC 3339.
        v: String,
    },
    Time {
        /// `HH:MM:SS`, 24h.
        v: String,
    },
    Glob {
        pattern: String,
    },
    Regex {
        src: String,
    },
    Range {
        start: i64,
        end: i64,
        inclusive: bool,
    },
    Task {
        id: u64,
        done: bool,
    },
    Closure {
        /// Display form; closures are not wire-invocable in v0.1.
        repr: String,
    },
    /// Stream chunks are deferred (site/content/internals/language-conformance-contract.md promises "ref + chunks"); today a
    /// stream only wires its label — pulling chunks needs a follow-up
    /// protocol method that does not exist yet.
    Stream {
        label: String,
    },
    /// Redaction by construction (site/content/internals/language-conformance-contract.md): never the secret material.
    Secret {
        name: String,
    },
    /// Alias / partial command application (`Value::CmdRef`). Not further
    /// structural in v0.1 — just its display form.
    Cmd {
        repr: String,
    },
    /// The elision rule (site/content/internals/kernel-protocol.md): withheld payload. Shape (type,
    /// count, table schema, a small preview, and a human-render head) always
    /// travels; the full value is fetchable via `value.get`/`shoal_get` on
    /// `uri` (with an explicit `elide` budget, or a field-path/slice).
    Ref {
        uri: String,
        of: String,
        n: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        cols: Option<BTreeMap<String, String>>,
        preview: Box<WireValue>,
        render_head: String,
    },
}

/// Byte-offset span into source, mirrors `shoal_ast::Span` on the wire
/// without pulling in an AST dependency here.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct WireSpan {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WirePath {
    pub display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

impl WirePath {
    pub fn encode(path: &OsStr) -> Self {
        let bytes = path.as_bytes();
        match std::str::from_utf8(bytes) {
            Ok(text) => Self {
                display: text.into(),
                raw: None,
            },
            Err(_) => Self {
                display: path.to_string_lossy().into_owned(),
                raw: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
            },
        }
    }
    pub fn decode(&self) -> Result<OsString, base64::DecodeError> {
        Ok(match &self.raw {
            Some(raw) => OsString::from_vec(base64::engine::general_purpose::STANDARD.decode(raw)?),
            None => OsString::from(&self.display),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientInfo {
    pub kind: String,
    pub tty: bool,
}
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task: Ref,
    pub session: String,
    pub state: String,
    pub started_ns: i64,
    pub finished_ns: Option<i64>,
    pub result_ref: Option<Ref>,
    pub error: Option<RpcError>,
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
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JournalQueryParams {
    pub since: Option<i64>,
    /// Upper time bound (ns since epoch); entries with `ts > until` are
    /// dropped. Filtered in the kernel, above the journal store.
    pub until: Option<i64>,
    pub principal: Option<String>,
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
    #[serde(default)]
    pub since: Option<u64>,
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames_are_newline_delimited() {
        let response = Response::ok(Value::from(1), serde_json::json!({"ok":true}));
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &response).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let decoded: Response = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, response);
    }

    #[test]
    fn read_frame_rejects_unterminated_unbounded_input() {
        struct Infinite;

        impl Read for Infinite {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                buf.fill(b'x');
                Ok(buf.len())
            }
        }

        let mut reader = io::BufReader::new(Infinite);
        let error = read_frame(&mut reader)
            .expect_err("an unterminated oversized frame must fail without unbounded buffering");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("16 MiB"), "{error}");
    }

    #[test]
    fn read_frame_rejects_a_single_oversized_line() {
        let mut body = "x".repeat(MAX_FRAME_LEN + 1024);
        body.push('\n');
        let mut reader = io::BufReader::new(body.as_bytes());
        let error = read_frame(&mut reader).expect_err("an oversized frame must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("16 MiB"), "{error}");
    }

    #[test]
    fn read_frame_still_reads_a_normal_frame() {
        let request = Request {
            jsonrpc: JSONRPC.into(),
            id: Value::from(1),
            method: "parse".into(),
            params: serde_json::json!({"src": "1 + 1"}),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let mut reader = io::BufReader::new(bytes.as_slice());
        assert_eq!(read_frame(&mut reader).unwrap(), Some(request));
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn non_utf8_path_roundtrips() {
        let original = OsString::from_vec(vec![b'a', 0xff, b'b']);
        let wire = WirePath::encode(&original);
        assert!(wire.raw.is_some());
        assert_eq!(wire.decode().unwrap(), original);
    }

    /// Locks the wire contract (refactor guard): every named `error_code`
    /// constant must keep meaning the exact numeric code shoal-kernel's
    /// handlers emitted before this taxonomy existed. Centralizing the
    /// constants must never silently renumber a code on the wire.
    #[test]
    fn error_code_constants_match_pinned_wire_values() {
        use error_code::*;
        assert_eq!(RPC_PARSE_ERROR, -32700);
        assert_eq!(INVALID_REQUEST, -32600);
        assert_eq!(METHOD_NOT_FOUND, -32601);
        assert_eq!(INVALID_PARAMS, -32602);
        assert_eq!(INTERNAL_ERROR, -32603);
        assert_eq!(NOT_ATTACHED, -32000);
        assert_eq!(PARSE_ERROR, -32001);
        assert_eq!(RAISED, -32002);
        assert_eq!(UNKNOWN_REF, -32004);
        assert_eq!(BAD_PATH_OR_SLICE, -32005);
        assert_eq!(LEASH_DENIED, -32010);
        assert_eq!(APPROVAL_REQUIRED, -32011);
        assert_eq!(UNKNOWN_PLAN, -32012);
        assert_eq!(TASK_CONTROL_UNAVAILABLE, -32020);
        assert_eq!(UNKNOWN_TASK, -32021);
        assert_eq!(UNKNOWN_PTY, -32022);
        assert_eq!(PTY_SPAWN_FAILED, -32023);
        assert_eq!(AUTH_FAILED, -32030);
    }
}
