//! `resources/*` handlers: `resources/list`, `resources/read`,
//! `resources/templates/list`, `resources/subscribe`, and the `shoal://` URI
//! parser they share (AGENT-SURFACE §1/§6/§8).

use crate::tools::bound_text;
use crate::{BridgeError, Facade, KernelClient, short_ref_to_uri};
use serde_json::{Value, json};

impl Facade {
    /// `resources/list` (AGENT-SURFACE §8): the stable roots plus per-session
    /// dynamic entries (open tasks). Values are browsed via `resources/read`.
    pub(crate) fn resources_list(&mut self) -> Result<Value, String> {
        let mut resources = vec![
            resource_entry("shoal://journal", "journal", "Structured execution journal"),
            resource_entry("shoal://jobs", "jobs", "The task table"),
            resource_entry("shoal://session/cwd", "cwd", "Session working directory"),
            resource_entry(
                "shoal://session/env",
                "env",
                "Session environment (names; values only if granted)",
            ),
            resource_entry(
                "shoal://session/reef",
                "reef",
                "Session reef resolution state (active scope + tool bindings)",
            ),
            resource_entry(
                "shoal://pty",
                "pty",
                "Open interactive PTY sessions (drill in via shoal://pty/{id})",
            ),
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
        // Dynamic: open plans this session/principal derived (inspectable via
        // `shoal://plan/{ref}`, applicable via `shoal_apply`).
        if let Ok(plans) = self.kernel.call("plan.list", json!({}))
            && let Some(array) = plans.as_array()
        {
            for plan in array {
                if let Some(plan_ref) = plan.get("plan_ref").and_then(Value::as_str) {
                    let uri = short_ref_to_uri(plan_ref);
                    resources.push(resource_entry(
                        &uri,
                        plan_ref,
                        "A derived plan: effects, reversibility, verdict",
                    ));
                }
            }
        }
        // Dynamic: open interactive ptys this session opened become live
        // resources — `shoal://pty/{id}` drills into one's rendered screen,
        // mirroring how a task/plan becomes an addressable noun.
        if let Ok(ptys) = self.kernel.call("pty.list", json!({}))
            && let Some(array) = ptys.get("ptys").and_then(Value::as_array)
        {
            for pty in array {
                if let Some(id) = pty.get("pty_id").and_then(Value::as_str) {
                    let uri = short_ref_to_uri(id);
                    resources.push(resource_entry(
                        &uri,
                        id,
                        "An open interactive PTY session (rendered screen)",
                    ));
                }
            }
        }
        Ok(json!({ "resources": resources }))
    }

    /// `resources/read` (AGENT-SURFACE §1/§8): dispatch a `shoal://` URI to the
    /// kernel and return `structuredContent` (the `$`-tagged / elided value).
    /// So an agent following an elided value's ref never hand-translates to
    /// `shoal_get`.
    pub(crate) fn resources_read(&mut self, params: Value) -> Result<Value, String> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or("resources/read requires uri")?
            .to_string();
        let parsed = ParsedUri::parse(&uri)?;
        // Session state views. `cwd` is served from the cached attach result (no
        // round-trip); `env`/`reef` read the session evaluator live via a
        // dedicated kernel method (fresh, so in-session `cd`/env-writes/reef
        // locks are reflected). Env is names-only unless granted (§1) — the
        // kernel enforces that; the facade just relays.
        if parsed.root == "session" {
            let field = parsed.segments.first().map(String::as_str).unwrap_or("");
            let value = match field {
                "cwd" => self
                    .kernel
                    .attach
                    .get("cwd")
                    .cloned()
                    .unwrap_or(Value::Null),
                "env" => self.call_kernel("session.env", json!({}))?,
                "reef" => self.call_kernel("session.reef", json!({}))?,
                other => return Err(format!("unsupported session view: {other}")),
            };
            return Ok(value_read_result(&uri, value));
        }
        // `shoal://task/{id}/out`: the task's captured output — the read side of
        // the subscription (§6). A kernel task captures the *whole* outcome at
        // completion (no streaming cursor infra yet), so this returns the full
        // current output: resolve the task record's `result_ref` through
        // `value.get` (reusing existing methods, honoring ?path/slice/format).
        // A task with no captured value yet (still running, or failed before
        // producing one) hands back its record so the reader sees state/error
        // rather than a misleading empty payload.
        if parsed.root == "task" && parsed.segments.get(1).map(String::as_str) == Some("out") {
            let id = parsed
                .segments
                .first()
                .ok_or("shoal://task/{id}/out needs an id")?;
            let record = self.call_kernel("task.get", json!({ "task": format!("task:{id}") }))?;
            let Some(result_ref) = record.get("result_ref").and_then(Value::as_str) else {
                return Ok(value_read_result(&uri, record));
            };
            let value = self.call_kernel(
                "value.get",
                json!({
                    "ref": result_ref,
                    "path": parsed.query.get("path"),
                    "slice": parsed.query.get("slice").and_then(|s| parse_slice(s)),
                    "format": parsed.query.get("format"),
                    "elide": parsed.query.get("elide").and_then(|s| serde_json::from_str::<Value>(s).ok()),
                }),
            )?;
            return Ok(value_read_result(&uri, value));
        }
        let (method, kparams) = parsed.to_kernel_call()?;
        let value = self.call_kernel(method, kparams)?;
        Ok(value_read_result(&uri, value))
    }

    /// One kernel JSON-RPC call with the resource layer's error convention: a
    /// kernel-side `error` surfaces as `kernel error: …`, transport failures as
    /// their own display. Shared by every `resources/read` path.
    fn call_kernel(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match self.kernel.call(method, params) {
            Ok(v) => Ok(v),
            Err(BridgeError::Kernel(e)) => Err(format!("kernel error: {e}")),
            Err(e) => Err(e.to_string()),
        }
    }

    /// `resources/subscribe` (AGENT-SURFACE §6): map a `shoal://events/{ch}` or
    /// `shoal://task/{id}/out` URI to a kernel channel and forward pushes as
    /// `notifications/resources/updated`. A dedicated background connection
    /// owns the subscription so it never contends with request/response reads.
    pub(crate) fn resources_subscribe(&mut self, params: Value) -> Result<Value, String> {
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

fn resource_entry(uri: &str, name: &str, description: &str) -> Value {
    json!({"uri":uri,"name":name,"description":description,"mimeType":"application/json"})
}

/// Turn a kernel value response into the MCP `resources/read` result shape.
/// `format=render`/`format=raw` responses carry a plain string — served as
/// `text/plain` instead of JSON-encoding a JSON-encoded string. A value URI
/// returns `{ref,value}`; unwrap to the value itself. Non-value payloads
/// (journal/jobs/events/session views/plans) travel verbatim as
/// `structuredContent`.
fn value_read_result(uri: &str, value: Value) -> Value {
    if let Some(text) = value
        .get("render")
        .or_else(|| value.get("raw"))
        .and_then(Value::as_str)
    {
        return json!({
            "contents": [{
                "uri": uri,
                "mimeType": "text/plain",
                "text": bound_text(text, None),
            }],
            "structuredContent": value,
        });
    }
    let contents = value.get("value").cloned().unwrap_or(value);
    json!({
        "contents": [{
            "uri": uri,
            "mimeType": "application/json",
            "text": bound_text(&serde_json::to_string_pretty(&contents).unwrap_or_default(), None),
        }],
        "structuredContent": contents,
    })
}

/// `resources/templates/list` (AGENT-SURFACE §8): the query-parameterized
/// forms an agent can instantiate.
pub(crate) fn resource_templates() -> Value {
    json!({"resourceTemplates":[
        {"uriTemplate":"shoal://out/{n}{?path,slice,format}","name":"transcript-value","description":"A transcript value, drillable by field-path/slice","mimeType":"application/json"},
        {"uriTemplate":"shoal://val/{hash}","name":"content-value","description":"An immutable content-addressed value","mimeType":"application/json"},
        {"uriTemplate":"shoal://task/{id}","name":"task","description":"A background task record","mimeType":"application/json"},
        {"uriTemplate":"shoal://task/{id}/out{?path,slice,format}","name":"task-output","description":"A task's captured output, drillable by field-path/slice","mimeType":"application/json"},
        {"uriTemplate":"shoal://plan/{ref}","name":"plan","description":"A derived plan: effects, reversibility, verdict","mimeType":"application/json"},
        {"uriTemplate":"shoal://session/{view}","name":"session-view","description":"A session state view: cwd | env | reef","mimeType":"application/json"},
        {"uriTemplate":"shoal://pty/{id}","name":"pty-screen","description":"An open interactive PTY session's rendered screen (same shape as pty.read)","mimeType":"application/json"},
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
                        "format": self.query.get("format"),
                    }),
                ))
            }
            "val" => {
                let hash = self
                    .segments
                    .first()
                    .ok_or("shoal://val/{hash} needs a hash")?;
                // Accept both the bare hash (`shoal://val/{hex}`) and the spec's
                // short-ref form `val:blake3:{hex}` → `shoal://val/blake3:{hex}`
                // (AGENT-SURFACE §1): the kernel's CAS keys on the raw hex, so
                // strip the `blake3:` algorithm prefix before `blob.get`.
                let hash = hash.strip_prefix("blake3:").unwrap_or(hash);
                Ok(("blob.get", json!({ "hash": hash })))
            }
            "plan" => {
                let plan_ref = self
                    .segments
                    .first()
                    .ok_or("shoal://plan/{ref} needs a plan ref")?;
                // The URI form drops the `plan:` prefix (`plan:<hex>` →
                // `shoal://plan/<hex>`); the kernel keys stored plans on the full
                // `plan:<hex>` ref, so restore it. Tolerate a caller that passes
                // the full ref back verbatim.
                let plan_ref = if plan_ref.starts_with("plan:") {
                    plan_ref.clone()
                } else {
                    format!("plan:{plan_ref}")
                };
                Ok(("plan.get", json!({ "plan_ref": plan_ref })))
            }
            "task" => {
                let id = self
                    .segments
                    .first()
                    .ok_or("shoal://task/{id} needs an id")?;
                Ok(("task.get", json!({ "task": format!("task:{id}") })))
            }
            "pty" => match self.segments.first() {
                // `shoal://pty` → the session's open pty list (same data as the
                // `pty.list` wire method / `shoal_pty_list` tool).
                None => Ok(("pty.list", json!({}))),
                // `shoal://pty/{id}` → that pty's rendered screen (same shape
                // `pty.read` returns). The URI form drops the `pty:` prefix
                // (`pty:{id}` → `shoal://pty/{id}`); the kernel keys live ptys
                // on the full `pty:{id}` ref, so restore it. Tolerate a caller
                // that passes the full ref back verbatim. A closed/unknown id
                // surfaces the kernel's clean UNKNOWN_PTY not-found.
                Some(id) => {
                    let pty_ref = if id.starts_with("pty:") {
                        id.clone()
                    } else {
                        format!("pty:{id}")
                    };
                    Ok(("pty.read", json!({ "pty_id": pty_ref })))
                }
            },
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

#[cfg(test)]
mod tests {
    use super::*;

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

        // The spec short-ref form `val:blake3:<hex>` → `shoal://val/blake3:<hex>`
        // must strip the algorithm prefix so the CAS lookup keys on raw hex.
        let val_prefixed = ParsedUri::parse("shoal://val/blake3:abc123").unwrap();
        let (method, params) = val_prefixed.to_kernel_call().unwrap();
        assert_eq!(method, "blob.get");
        assert_eq!(params["hash"], "abc123");

        // A plan URI drops the `plan:` prefix; the kernel keys on the full ref.
        let plan = ParsedUri::parse("shoal://plan/deadbeef00112233").unwrap();
        let (method, params) = plan.to_kernel_call().unwrap();
        assert_eq!(method, "plan.get");
        assert_eq!(params["plan_ref"], "plan:deadbeef00112233");

        // `shoal://pty` enumerates; `shoal://pty/{id}` reads one rendered
        // screen, restoring the `pty:` ref prefix the URI form drops.
        let pty_root = ParsedUri::parse("shoal://pty").unwrap();
        let (method, params) = pty_root.to_kernel_call().unwrap();
        assert_eq!(method, "pty.list");
        assert_eq!(params, json!({}));
        let pty_one = ParsedUri::parse("shoal://pty/3").unwrap();
        let (method, params) = pty_one.to_kernel_call().unwrap();
        assert_eq!(method, "pty.read");
        assert_eq!(params["pty_id"], "pty:3");
        // A caller that passes the full ref back verbatim is tolerated.
        let pty_full = ParsedUri::parse("shoal://pty/pty:3").unwrap();
        let (_, params) = pty_full.to_kernel_call().unwrap();
        assert_eq!(params["pty_id"], "pty:3");
        // A pty URI is not subscribable (the push event is a documented
        // follow-up, not wired here).
        assert!(pty_one.event_channel().is_none());
    }
}
