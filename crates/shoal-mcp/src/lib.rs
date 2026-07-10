//! MCP stdio facade for the shoal kernel protocol.

use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub socket: PathBuf,
    pub session: Option<String>,
    pub token: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let session = std::env::var("SHOAL_SESSION").ok();
        let socket = discover_socket(session.as_deref().unwrap_or("default"));
        Ok(Self {
            socket,
            session,
            token: std::env::var("SHOAL_TOKEN").ok(),
        })
    }
}

/// Resolve the kernel socket the SAME way `shoal-kernel` does, so discovery
/// works cross-platform — in particular on macOS, where `XDG_RUNTIME_DIR` is
/// unset by default and the kernel falls back to `/tmp/shoal-{uid}`. Order:
///
/// 1. `SHOAL_SOCKET` (explicit override) — used verbatim.
/// 2. `$XDG_RUNTIME_DIR/shoal/{session}.sock`.
/// 3. `$TMPDIR/shoal-{uid}/shoal/{session}.sock` (macOS sets `TMPDIR`).
/// 4. `/tmp/shoal-{uid}/shoal/{session}.sock` (kernel's own final fallback).
///
/// Without this, a bare `XDG_RUNTIME_DIR`-only lookup silently failed on macOS
/// and socket discovery never found the running kernel.
pub fn discover_socket(session: &str) -> PathBuf {
    if let Some(explicit) = std::env::var_os("SHOAL_SOCKET").filter(|s| !s.is_empty()) {
        return PathBuf::from(explicit);
    }
    runtime_dir().join("shoal").join(format!("{session}.sock"))
}

/// The runtime directory the kernel binds its socket under. Mirrors
/// `shoal-kernel`'s `runtime_socket`, with a `$TMPDIR` step so a macOS session
/// that exports `TMPDIR` (but not `XDG_RUNTIME_DIR`) is honored before the
/// hard `/tmp/shoal-{uid}` fallback.
fn runtime_dir() -> PathBuf {
    runtime_dir_from(
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("TMPDIR"),
        unsafe { geteuid() },
    )
}

/// Pure socket-directory selection (kept separate so the macOS no-`XDG` case is
/// unit-testable without mutating process env): `$XDG_RUNTIME_DIR`, else
/// `$TMPDIR/shoal-{uid}`, else `/tmp/shoal-{uid}` — identical to shoal-kernel.
fn runtime_dir_from(
    xdg: Option<std::ffi::OsString>,
    tmpdir: Option<std::ffi::OsString>,
    uid: u32,
) -> PathBuf {
    if let Some(xdg) = xdg.filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg);
    }
    if let Some(tmp) = tmpdir.filter(|s| !s.is_empty()) {
        return PathBuf::from(tmp).join(format!("shoal-{uid}"));
    }
    PathBuf::from(format!("/tmp/shoal-{uid}"))
}

unsafe extern "C" {
    fn geteuid() -> u32;
}

pub struct KernelClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    attach: Value,
}

impl KernelClient {
    pub fn connect(config: &Config) -> Result<Self, BridgeError> {
        let stream = UnixStream::connect(&config.socket)?;
        let mut client = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_id: 1,
            attach: Value::Null,
        };
        client.attach = client.call(
            "session.attach",
            json!({
                "session": config.session,
                "token": config.token,
                "client": {"kind":"mcp", "tty":false}
            }),
        )?;
        Ok(client)
    }

    /// Subscribe on this (dedicated) connection and forward every pushed
    /// `event` notification to MCP stdout as `notifications/resources/updated`
    /// (AGENT-SURFACE §6). Runs until the connection closes.
    fn run_event_forwarder(mut self, channel: String, uri: String) {
        if self
            .call("events.subscribe", json!({"channel": channel}))
            .is_err()
        {
            return;
        }
        while let Ok(Some(frame)) = read_json_line(&mut self.reader) {
            if frame.get("method").and_then(Value::as_str) == Some("event") {
                let p = frame.get("params").cloned().unwrap_or(Value::Null);
                let note = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/resources/updated",
                    "params": {
                        "uri": uri,
                        "seq": p.get("seq"),
                        "payload": p.get("payload"),
                    }
                });
                let _ = write_stdout_frame(&note);
            }
        }
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, BridgeError> {
        let id = self.next_id;
        self.next_id += 1;
        write_json_line(
            &mut self.writer,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        loop {
            let frame = read_json_line(&mut self.reader)?.ok_or(BridgeError::Disconnected)?;
            // Kernel notifications can be interleaved with the response.
            if frame.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = frame.get("error") {
                return Err(BridgeError::Kernel(error.clone()));
            }
            return frame.get("result").cloned().ok_or_else(|| {
                BridgeError::Protocol("kernel response has neither result nor error".into())
            });
        }
    }
}

#[derive(Debug)]
pub enum BridgeError {
    Io(io::Error),
    Json(serde_json::Error),
    Protocol(String),
    Kernel(Value),
    Disconnected,
}
impl From<io::Error> for BridgeError {
    fn from(v: io::Error) -> Self {
        Self::Io(v)
    }
}
impl From<serde_json::Error> for BridgeError {
    fn from(v: serde_json::Error) -> Self {
        Self::Json(v)
    }
}
impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "{e}"),
            Self::Protocol(e) => write!(f, "{e}"),
            Self::Kernel(e) => write!(f, "kernel error: {e}"),
            Self::Disconnected => write!(f, "kernel disconnected"),
        }
    }
}
impl std::error::Error for BridgeError {}

pub struct Facade {
    kernel: KernelClient,
    config: Config,
}
impl Facade {
    pub fn connect(config: &Config) -> Result<Self, BridgeError> {
        Ok(Self {
            kernel: KernelClient::connect(config)?,
            config: config.clone(),
        })
    }
    pub fn handle(&mut self, request: &Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str);
        // MCP notifications intentionally have no response.
        let id = id?;
        if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(rpc_error(id, -32600, "invalid JSON-RPC request", None));
        }
        let result = match method {
            Some("initialize") => Ok(
                json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false},"resources":{"subscribe":true,"listChanged":false}},"serverInfo":{"name":"shoal-mcp","version":env!("CARGO_PKG_VERSION")}}),
            ),
            Some("ping") => Ok(json!({})),
            Some("tools/list") => Ok(json!({"tools":tools()})),
            Some("tools/call") => {
                self.tools_call(request.get("params").cloned().unwrap_or(Value::Null))
            }
            Some("resources/list") => self.resources_list(),
            Some("resources/templates/list") => Ok(resource_templates()),
            Some("resources/read") => {
                self.resources_read(request.get("params").cloned().unwrap_or(Value::Null))
            }
            Some("resources/subscribe") => {
                self.resources_subscribe(request.get("params").cloned().unwrap_or(Value::Null))
            }
            Some("resources/unsubscribe") => Ok(json!({})),
            Some(m) => {
                return Some(rpc_error(
                    id,
                    -32601,
                    "method not found",
                    Some(json!({"method":m})),
                ));
            }
            None => {
                return Some(rpc_error(
                    id,
                    -32600,
                    "request method must be a string",
                    None,
                ));
            }
        };
        Some(match result {
            Ok(v) => json!({"jsonrpc":"2.0","id":id,"result":v}),
            Err(e) => rpc_error(id, -32602, &e, None),
        })
    }

    fn tools_call(&mut self, params: Value) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or("tools/call requires name")?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let (method, kparams) = map_tool(name, args)?;
        match self.kernel.call(method, kparams) {
            Ok(result) => Ok(tool_result(result, false)),
            Err(BridgeError::Kernel(error)) => Ok(tool_result(error, true)),
            Err(error) => Err(error.to_string()),
        }
    }

    /// `resources/list` (AGENT-SURFACE §8): the stable roots plus per-session
    /// dynamic entries (open tasks). Values are browsed via `resources/read`.
    fn resources_list(&mut self) -> Result<Value, String> {
        let mut resources = vec![
            resource_entry("shoal://journal", "journal", "Structured execution journal"),
            resource_entry("shoal://jobs", "jobs", "The task table"),
            resource_entry("shoal://session/cwd", "cwd", "Session working directory"),
        ];
        // Dynamic: open/recent tasks become live resources.
        if let Ok(tasks) = self.kernel.call("task.list", json!({}))
            && let Some(array) = tasks.as_array()
        {
            for task in array {
                if let Some(id) = task.get("task").and_then(Value::as_str) {
                    let uri = short_ref_to_uri(id);
                    resources.push(resource_entry(&uri, id, "Background task record"));
                }
            }
        }
        Ok(json!({ "resources": resources }))
    }

    /// `resources/read` (AGENT-SURFACE §1/§8): dispatch a `shoal://` URI to the
    /// kernel and return `structuredContent` (the `$`-tagged / elided value).
    /// So an agent following an elided value's ref never hand-translates to
    /// `shoal_get`.
    fn resources_read(&mut self, params: Value) -> Result<Value, String> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or("resources/read requires uri")?
            .to_string();
        let parsed = ParsedUri::parse(&uri)?;
        // Session state views are served from the cached attach result — no
        // extra round-trip, and env is names-only unless granted (§1).
        if parsed.root == "session" {
            let field = parsed.segments.first().map(String::as_str).unwrap_or("");
            let contents = match field {
                "cwd" => self
                    .kernel
                    .attach
                    .get("cwd")
                    .cloned()
                    .unwrap_or(Value::Null),
                other => return Err(format!("unsupported session view: {other}")),
            };
            return Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": serde_json::to_string_pretty(&contents).unwrap_or_default(),
                }],
                "structuredContent": contents,
            }));
        }
        let (method, kparams) = parsed.to_kernel_call()?;
        let value = match self.kernel.call(method, kparams) {
            Ok(v) => v,
            Err(BridgeError::Kernel(e)) => return Err(format!("kernel error: {e}")),
            Err(e) => return Err(e.to_string()),
        };
        // A value URI returns `{ref,value}`; unwrap to the value itself for the
        // resource contents. Non-value roots (journal/jobs/events) return their
        // structured payload verbatim.
        let contents = value.get("value").cloned().unwrap_or(value);
        Ok(json!({
            "contents": [{
                "uri": uri,
                "mimeType": "application/json",
                "text": bound_text(&serde_json::to_string_pretty(&contents).unwrap_or_default(), None),
            }],
            "structuredContent": contents,
        }))
    }

    /// `resources/subscribe` (AGENT-SURFACE §6): map a `shoal://events/{ch}` or
    /// `shoal://task/{id}/out` URI to a kernel channel and forward pushes as
    /// `notifications/resources/updated`. A dedicated background connection
    /// owns the subscription so it never contends with request/response reads.
    fn resources_subscribe(&mut self, params: Value) -> Result<Value, String> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or("resources/subscribe requires uri")?
            .to_string();
        let channel = ParsedUri::parse(&uri)?
            .event_channel()
            .ok_or("only shoal://events/{ch} and shoal://task/{id}[/out] are subscribable")?;
        let config = self.config.clone();
        let forward_uri = uri.clone();
        match KernelClient::connect(&config) {
            Ok(client) => {
                std::thread::spawn(move || client.run_event_forwarder(channel, forward_uri));
                Ok(json!({}))
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

pub fn run_stdio(config: &Config) -> Result<(), BridgeError> {
    let mut facade = Facade::connect(config)?;
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    // stdout is written per-frame under its own lock (via `write_stdout_frame`)
    // rather than held for the whole loop, so a subscription forwarder thread
    // can also push notification frames without deadlocking on the writer.
    loop {
        match read_json_line(&mut reader) {
            Ok(Some(request)) => {
                if let Some(response) = facade.handle(&request) {
                    write_stdout_frame(&response)?
                }
            }
            Ok(None) => return Ok(()),
            Err(BridgeError::Json(error)) => write_stdout_frame(&rpc_error(
                Value::Null,
                -32700,
                "parse error",
                Some(json!({"detail":error.to_string()})),
            ))?,
            Err(error) => return Err(error),
        }
    }
}

/// Write one newline-framed JSON value to stdout atomically under the stdout
/// lock. Both the main loop and any subscription forwarder use this, so their
/// frames never interleave on the wire.
fn write_stdout_frame(value: &Value) -> Result<(), BridgeError> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(&buf)?;
    handle.flush()?;
    Ok(())
}

fn map_tool(name: &str, args: Value) -> Result<(&'static str, Value), String> {
    let object = args.as_object().ok_or("tool arguments must be an object")?;
    Ok(match name {
        // AGENT-SURFACE §5 exec signature: mode/position/background/timeout_ms/
        // elide are all forwarded (no more silently-dropped params).
        "shoal_exec" => (
            "exec",
            json!({
                "src": required_str(object,"src")?,
                "mode": object.get("mode").and_then(Value::as_str).unwrap_or("run"),
                "position": object.get("position").and_then(Value::as_str).unwrap_or("value"),
                "background": object.get("background").and_then(Value::as_bool).unwrap_or(false),
                "timeout_ms": object.get("timeout_ms"),
                "elide": object.get("elide"),
            }),
        ),
        "shoal_plan" => (
            "exec",
            json!({"src":required_str(object,"src")?,"mode":"plan","position":"value"}),
        ),
        "shoal_apply" => (
            "plan.apply",
            json!({"plan_ref":required_str(object,"plan_ref")?}),
        ),
        // Forward the per-call elide budget so a caller can tighten/loosen it
        // (AGENT-SURFACE §3) — previously accepted but dropped.
        "shoal_get" => (
            "value.get",
            json!({
                "ref": required_str(object,"ref")?,
                "path": object.get("path"),
                "slice": object.get("slice"),
                "elide": object.get("elide"),
            }),
        ),
        // `until`/`effects`/`ok` are honored kernel-side; forward verbatim.
        "shoal_journal" => ("journal.query", args),
        // Task cancellation (AGENT-SURFACE §5).
        "shoal_cancel" => ("task.cancel", json!({"task": required_str(object,"task")?})),
        // Escalation path for a plan stuck at `approval_pending` (TDD §7's
        // `cap.request`): without this an agent that hits a stricter-than-
        // default leash policy has no MCP-reachable way to move forward.
        // `effects` scopes the grant (AGENT-SURFACE §5).
        "shoal_cap_request" => (
            "cap.request",
            json!({"plan_ref":required_str(object,"plan_ref")?,"effects":object.get("effects").cloned().unwrap_or_else(||json!([]))}),
        ),
        _ => return Err(format!("unknown tool {name:?}")),
    })
}
fn required_str<'a>(o: &'a serde_json::Map<String, Value>, name: &str) -> Result<&'a str, String> {
    o.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string argument {name:?}"))
}
/// Absolute per-result cap for text/render sent to the agent (AGENT-SURFACE
/// §3). A misbehaving agent cannot flood its own context: no render or text
/// content ever exceeds this, regardless of the value's size.
const RESULT_TEXT_HARD_CAP: usize = 64 * 1024;

/// Build a `tools/call` result whose context footprint is bounded
/// (AGENT-SURFACE §1/§3): `structuredContent` carries the kernel's already-
/// elided value; the human-readable `text` content is the render **head**,
/// truncated with a `…(N more lines, fetch via ref)` marker; and a
/// `resource_link` points at the value's ref so the agent can drill in for
/// zero tokens rather than receiving the payload inline.
fn tool_result(value: Value, is_error: bool) -> Value {
    // Find the ref/uri this result is addressable by, for the link + marker.
    let uri = value
        .get("uri")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| {
            value
                .get("value")
                .and_then(|v| v.get("uri"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        .or_else(|| {
            value
                .get("ref")
                .and_then(Value::as_str)
                .map(short_ref_to_uri)
        });
    // Prefer the kernel's human render; fall back to compact JSON. Either way
    // it is bounded — the render string is NOT trusted to be small (a 150-row
    // table renders to many KiB), which is exactly the elision bypass §3 closes.
    let text = match value.get("render").and_then(Value::as_str) {
        Some(render) => bound_text(render, uri.as_deref()),
        None => bound_text(
            &serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".into()),
            uri.as_deref(),
        ),
    };
    let mut content = vec![json!({"type":"text","text":text})];
    if let Some(uri) = &uri {
        content.push(json!({"type":"resource_link","uri":uri}));
    }
    json!({"content":content,"structuredContent":value,"isError":is_error})
}

/// Bound `text` to the hard cap by keeping a head of whole lines and appending
/// a `…(N more lines, fetch via ref)` marker. Never returns more than
/// `RESULT_TEXT_HARD_CAP` bytes.
fn bound_text(text: &str, uri: Option<&str>) -> String {
    if text.len() <= RESULT_TEXT_HARD_CAP {
        return text.to_string();
    }
    let budget = RESULT_TEXT_HARD_CAP.saturating_sub(96);
    let total_lines = text.lines().count();
    let mut head = String::new();
    let mut kept = 0usize;
    for line in text.lines() {
        if head.len() + line.len() + 1 > budget {
            break;
        }
        head.push_str(line);
        head.push('\n');
        kept += 1;
    }
    // Degenerate case: a single line longer than the budget — hard byte cut.
    if head.is_empty() {
        head = text.chars().take(budget).collect();
    }
    let remaining = total_lines.saturating_sub(kept);
    let via = match uri {
        Some(uri) => format!("fetch via {uri}"),
        None => "fetch via ref".into(),
    };
    format!("{head}…({remaining} more lines, {via})")
}
fn rpc_error(id: Value, code: i32, message: &str, data: Option<Value>) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message,"data":data}})
}

/// `kind:id` short ref → `shoal://kind/id` URI (AGENT-SURFACE §1).
fn short_ref_to_uri(short: &str) -> String {
    match short.split_once(':') {
        Some((kind, rest)) => format!("shoal://{kind}/{rest}"),
        None => format!("shoal://{short}"),
    }
}

fn resource_entry(uri: &str, name: &str, description: &str) -> Value {
    json!({"uri":uri,"name":name,"description":description,"mimeType":"application/json"})
}

/// `resources/templates/list` (AGENT-SURFACE §8): the query-parameterized
/// forms an agent can instantiate.
fn resource_templates() -> Value {
    json!({"resourceTemplates":[
        {"uriTemplate":"shoal://out/{n}{?path,slice,format}","name":"transcript-value","description":"A transcript value, drillable by field-path/slice","mimeType":"application/json"},
        {"uriTemplate":"shoal://val/{hash}","name":"content-value","description":"An immutable content-addressed value","mimeType":"application/json"},
        {"uriTemplate":"shoal://task/{id}","name":"task","description":"A background task record","mimeType":"application/json"},
        {"uriTemplate":"shoal://journal{?since,until,head,principal,ok,effects,limit}","name":"journal","description":"The structured execution journal","mimeType":"application/json"},
        {"uriTemplate":"shoal://events/{channel}{?since,limit}","name":"events","description":"A cursor-read event channel","mimeType":"application/json"}
    ]})
}

/// A parsed `shoal://` resource URI (AGENT-SURFACE §1).
struct ParsedUri {
    root: String,
    segments: Vec<String>,
    query: std::collections::HashMap<String, String>,
}

impl ParsedUri {
    fn parse(uri: &str) -> Result<Self, String> {
        let rest = uri
            .strip_prefix("shoal://")
            .ok_or_else(|| format!("not a shoal:// uri: {uri}"))?;
        let (path, query_str) = match rest.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (rest, None),
        };
        let mut segments: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(percent_decode)
            .collect();
        if segments.is_empty() {
            return Err(format!("empty shoal:// uri: {uri}"));
        }
        let root = segments.remove(0);
        let mut query = std::collections::HashMap::new();
        if let Some(q) = query_str {
            for pair in q.split('&').filter(|p| !p.is_empty()) {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                query.insert(k.to_string(), percent_decode(v));
            }
        }
        Ok(Self {
            root,
            segments,
            query,
        })
    }

    /// The kernel JSON-RPC call this resource read maps to.
    fn to_kernel_call(&self) -> Result<(&'static str, Value), String> {
        match self.root.as_str() {
            "out" => {
                let n = self
                    .segments
                    .first()
                    .ok_or("shoal://out/{n} needs an index")?;
                let slice = self.query.get("slice").and_then(|s| parse_slice(s));
                Ok((
                    "value.get",
                    json!({
                        "ref": format!("out:{n}"),
                        "path": self.query.get("path"),
                        "slice": slice,
                        "elide": self.query.get("elide").and_then(|s| serde_json::from_str::<Value>(s).ok()),
                    }),
                ))
            }
            "val" => {
                let hash = self
                    .segments
                    .first()
                    .ok_or("shoal://val/{hash} needs a hash")?;
                Ok(("blob.get", json!({ "hash": hash })))
            }
            "task" => {
                let id = self
                    .segments
                    .first()
                    .ok_or("shoal://task/{id} needs an id")?;
                Ok(("task.get", json!({ "task": format!("task:{id}") })))
            }
            "jobs" => Ok(("task.list", json!({}))),
            "journal" => Ok((
                "journal.query",
                json!({
                    "since": self.query.get("since").and_then(|s| s.parse::<i64>().ok()),
                    "until": self.query.get("until").and_then(|s| s.parse::<i64>().ok()),
                    "head": self.query.get("head"),
                    "principal": self.query.get("principal"),
                    "ok": self.query.get("ok").and_then(|s| s.parse::<bool>().ok()),
                    "effects": self.query.get("effects").map(|s| s.split(',').map(String::from).collect::<Vec<_>>()),
                    "limit": self.query.get("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0),
                }),
            )),
            "events" => {
                let channel = self
                    .segments
                    .first()
                    .ok_or("shoal://events/{channel} needs a channel")?;
                Ok((
                    "events.read",
                    json!({
                        "channel": channel,
                        "since": self.query.get("since").and_then(|s| s.parse::<u64>().ok()),
                        "limit": self.query.get("limit").and_then(|s| s.parse::<usize>().ok()),
                    }),
                ))
            }
            other => Err(format!("unsupported resource root: {other}")),
        }
    }

    /// The kernel channel this URI subscribes to, if any.
    fn event_channel(&self) -> Option<String> {
        match self.root.as_str() {
            "events" => self.segments.first().cloned(),
            "task" => self.segments.first().map(|id| format!("task.{id}")),
            _ => None,
        }
    }
}

fn parse_slice(s: &str) -> Option<Value> {
    let (a, b) = s.split_once("..")?;
    Some(json!([a.parse::<usize>().ok()?, b.parse::<usize>().ok()?]))
}

/// Minimal percent-decoding for resource URIs (enough for `%20`, `%2F`, etc.).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn tools() -> Vec<Value> {
    vec![
        tool(
            "shoal_exec",
            "Execute shoal source and return a stable transcript reference (or a task ref when background/timed-out)",
            json!({"type":"object","properties":{"src":{"type":"string"},"mode":{"enum":["run","plan"]},"position":{"enum":["stmt","value"]},"background":{"type":"boolean"},"timeout_ms":{"type":"integer","minimum":1},"elide":{"type":"object","properties":{"max_bytes":{"type":"integer"},"max_rows":{"type":"integer"},"max_items":{"type":"integer"}}}},"required":["src"],"additionalProperties":false}),
        ),
        tool(
            "shoal_plan",
            "Derive concrete effects and reversibility without spawning",
            json!({"type":"object","properties":{"src":{"type":"string"}},"required":["src"],"additionalProperties":false}),
        ),
        tool(
            "shoal_apply",
            "Apply a previously approved plan",
            json!({"type":"object","properties":{"plan_ref":{"type":"string"}},"required":["plan_ref"],"additionalProperties":false}),
        ),
        tool(
            "shoal_get",
            "Query or slice a transcript value without re-execution",
            json!({"type":"object","properties":{"ref":{"type":"string"},"path":{"type":"string"},"slice":{"type":"array","items":{"type":"integer"},"minItems":2,"maxItems":2},"elide":{"type":"object","properties":{"max_bytes":{"type":"integer"},"max_rows":{"type":"integer"},"max_items":{"type":"integer"}}}},"required":["ref"],"additionalProperties":false}),
        ),
        tool(
            "shoal_journal",
            "Query the structured execution journal",
            json!({"type":"object","properties":{"since":{"type":"integer"},"until":{"type":"integer"},"principal":{"type":"string"},"ok":{"type":"boolean"},"effects":{"type":"array","items":{"type":"string"}},"head":{"type":"string"},"limit":{"type":"integer","minimum":1}},"additionalProperties":false}),
        ),
        tool(
            "shoal_cancel",
            "Request cancellation of a background task",
            json!({"type":"object","properties":{"task":{"type":"string"}},"required":["task"],"additionalProperties":false}),
        ),
        tool(
            "shoal_cap_request",
            "Request grant/approval for a plan stuck at approval_pending; effects scope the grant",
            json!({"type":"object","properties":{"plan_ref":{"type":"string"},"effects":{"type":"array"}},"required":["plan_ref"],"additionalProperties":false}),
        ),
    ]
}
fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name":name,"description":description,"inputSchema":input_schema})
}

fn read_json_line<R: BufRead>(reader: &mut R) -> Result<Option<Value>, BridgeError> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_FRAME {
        return Err(BridgeError::Protocol("frame exceeds 16 MiB".into()));
    }
    Ok(Some(serde_json::from_str(line.trim_end())?))
}
fn write_json_line<W: Write>(writer: &mut W, value: &Value) -> Result<(), BridgeError> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub fn socket_exists(path: &Path) -> bool {
    fs_type(path).is_some_and(|t| t.is_socket())
}

fn fs_type(path: &Path) -> Option<std::fs::FileType> {
    std::fs::metadata(path).ok().map(|m| m.file_type())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;
    fn mock() -> (tempfile::TempDir, Config, thread::JoinHandle<Vec<Value>>) {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("kernel.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let h = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut w = stream;
            let mut seen = vec![];
            for n in 0..2 {
                let req = read_json_line(&mut r).unwrap().unwrap();
                seen.push(req.clone());
                let id = req["id"].clone();
                let result = if n == 0 {
                    json!({"session":"s","principal":"human","caps":{},"cwd":{"display":"/tmp"},"env_hash":"x","ast_version":1})
                } else {
                    json!({"ref":"out:1","value":{"$":"int","v":3}})
                };
                write_json_line(&mut w, &json!({"jsonrpc":"2.0","id":id,"result":result})).unwrap()
            }
            seen
        });
        let c = Config {
            socket: path,
            session: Some("s".into()),
            token: Some("tok".into()),
        };
        (d, c, h)
    }
    #[test]
    fn lists_all_tools() {
        let names: Vec<String> = tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"shoal_cancel".to_string()));
        assert!(names.contains(&"shoal_exec".to_string()));
        assert_eq!(tools().len(), 7);
        for t in tools() {
            assert_eq!(t["inputSchema"]["type"], "object")
        }
    }
    #[test]
    fn facade_attaches_and_maps_exec() {
        let (_d, c, h) = mock();
        let mut f = Facade::connect(&c).unwrap();
        let response=f.handle(&json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"shoal_exec","arguments":{"src":"1+2"}}})).unwrap();
        assert_eq!(response["result"]["structuredContent"]["ref"], "out:1");
        let seen = h.join().unwrap();
        assert_eq!(seen[0]["method"], "session.attach");
        assert_eq!(seen[0]["params"]["token"], "tok");
        assert_eq!(seen[1]["method"], "exec");
        assert_eq!(seen[1]["params"]["mode"], "run");
    }
    #[test]
    fn maps_all_tools() {
        assert_eq!(
            map_tool("shoal_plan", json!({"src":"rm x"})).unwrap().0,
            "exec"
        );
        assert_eq!(
            map_tool("shoal_apply", json!({"plan_ref":"plan:x"}))
                .unwrap()
                .0,
            "plan.apply"
        );
        assert_eq!(
            map_tool("shoal_get", json!({"ref":"out:1"})).unwrap().0,
            "value.get"
        );
        assert_eq!(
            map_tool("shoal_journal", json!({"limit":2})).unwrap().0,
            "journal.query"
        );
        assert_eq!(
            map_tool("shoal_cap_request", json!({"plan_ref":"plan:x"}))
                .unwrap()
                .0,
            "cap.request"
        );
    }
    #[test]
    fn cap_request_forwards_plan_ref() {
        let (method, params) = map_tool("shoal_cap_request", json!({"plan_ref":"plan:x"})).unwrap();
        assert_eq!(method, "cap.request");
        assert_eq!(params["plan_ref"], "plan:x");
        assert_eq!(params["effects"], json!([]));
    }
    #[test]
    fn protocol_errors_are_structured() {
        let (_d, c, h) = mock();
        let mut f = Facade::connect(&c).unwrap();
        let e = f
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"nope"}))
            .unwrap();
        assert_eq!(e["error"]["code"], -32601);
        drop(f);
        let _ = h.join();
    }
    #[test]
    fn socket_probe_is_truthful() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        assert!(!socket_exists(&p));
        let _l = UnixListener::bind(&p).unwrap();
        assert!(socket_exists(&p));
    }

    /// macOS-first-class socket discovery: with no `XDG_RUNTIME_DIR` (the macOS
    /// default), the path must fall through exactly as shoal-kernel does — to
    /// `$TMPDIR/shoal-{uid}` when `TMPDIR` is set, else `/tmp/shoal-{uid}`.
    #[test]
    fn socket_discovery_falls_back_without_xdg() {
        use std::ffi::OsString;
        // No XDG, no TMPDIR → hard /tmp fallback.
        assert_eq!(
            runtime_dir_from(None, None, 501),
            PathBuf::from("/tmp/shoal-501")
        );
        // No XDG, TMPDIR set (the macOS shape) → $TMPDIR/shoal-{uid}.
        assert_eq!(
            runtime_dir_from(None, Some(OsString::from("/var/folders/xy")), 501),
            PathBuf::from("/var/folders/xy/shoal-501")
        );
        // XDG present → used verbatim (Linux).
        assert_eq!(
            runtime_dir_from(
                Some(OsString::from("/run/user/1000")),
                Some(OsString::from("/tmp")),
                1000
            ),
            PathBuf::from("/run/user/1000")
        );
        // Empty XDG is treated as unset (a common shell footgun).
        assert_eq!(
            runtime_dir_from(Some(OsString::new()), None, 7),
            PathBuf::from("/tmp/shoal-7")
        );
    }

    /// The elision doctrine (§3) at the MCP boundary: a huge render/text is
    /// never emitted whole — it is truncated to a head with a fetch marker and
    /// stays under the 64 KiB hard cap, so the render string can't bypass the
    /// wall the structured value already respects.
    #[test]
    fn bound_text_truncates_large_render_with_marker() {
        let big = (0..5000)
            .map(|i| format!("row {i:04} ....................................."))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(big.len() > RESULT_TEXT_HARD_CAP);
        let bounded = bound_text(&big, Some("shoal://out/9"));
        assert!(
            bounded.len() <= RESULT_TEXT_HARD_CAP,
            "bounded text must respect the hard cap, was {}",
            bounded.len()
        );
        assert!(bounded.contains("more lines, fetch via shoal://out/9"));
        // A small render passes through untouched.
        assert_eq!(bound_text("hi\nthere", None), "hi\nthere");
    }

    /// A `tools/call` result never puts the payload in the text content: the
    /// render head is bounded and a `resource_link` points at the ref.
    #[test]
    fn tool_result_bounds_render_and_links_the_ref() {
        let huge_render = "x".repeat(200_000);
        let result = tool_result(
            json!({"ref":"out:12","value":{"$":"ref","uri":"shoal://out/12"},"render":huge_render}),
            false,
        );
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.len() <= RESULT_TEXT_HARD_CAP);
        let has_link = result["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["type"] == "resource_link" && c["uri"] == "shoal://out/12");
        assert!(has_link, "result must carry a resource_link to the ref");
    }

    #[test]
    fn parses_resource_uris_to_kernel_calls() {
        let out = ParsedUri::parse("shoal://out/12?path=.rows[3].name").unwrap();
        let (method, params) = out.to_kernel_call().unwrap();
        assert_eq!(method, "value.get");
        assert_eq!(params["ref"], "out:12");
        assert_eq!(params["path"], ".rows[3].name");

        let ev = ParsedUri::parse("shoal://events/user.deploy?since=2&limit=10").unwrap();
        assert_eq!(ev.event_channel().as_deref(), Some("user.deploy"));
        let (method, params) = ev.to_kernel_call().unwrap();
        assert_eq!(method, "events.read");
        assert_eq!(params["channel"], "user.deploy");
        assert_eq!(params["since"], 2);

        let task = ParsedUri::parse("shoal://task/7/out").unwrap();
        assert_eq!(task.event_channel().as_deref(), Some("task.7"));

        let val = ParsedUri::parse("shoal://val/abc123").unwrap();
        let (method, params) = val.to_kernel_call().unwrap();
        assert_eq!(method, "blob.get");
        assert_eq!(params["hash"], "abc123");
    }
}
