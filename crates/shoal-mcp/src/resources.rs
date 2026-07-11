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
    pub(crate) fn resources_read(&mut self, params: Value) -> Result<Value, String> {
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

/// `resources/templates/list` (AGENT-SURFACE §8): the query-parameterized
/// forms an agent can instantiate.
pub(crate) fn resource_templates() -> Value {
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
    }
}
