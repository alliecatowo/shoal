//! `resources/*` handlers: `resources/list`, `resources/read`,
//! `resources/templates/list`, `resources/subscribe`, and the `shoal://` URI
//! parser they share (site/content/internals/kernel-protocol.md).

use crate::tools::{bound_text, value_within_admission};
use crate::{BridgeError, Facade, short_ref_to_uri};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub(crate) const MAX_RESOURCE_URI_BYTES: usize = 4 * 1024;
const MAX_RESOURCE_SEGMENTS: usize = 4;
const MAX_RESOURCE_SEGMENT_BYTES: usize = 512;
const MAX_RESOURCE_QUERY_PAIRS: usize = 16;
const MAX_RESOURCE_QUERY_KEY_BYTES: usize = 64;
const MAX_RESOURCE_QUERY_VALUE_BYTES: usize = 2 * 1024;
const MAX_RESOURCE_RESULT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn resource_uri_admitted(uri: &str) -> bool {
    ParsedUri::parse(uri).is_ok()
}

impl Facade {
    /// `resources/list` (site/content/internals/kernel-protocol.md): the stable roots plus per-session
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
                if let Some(id) = task.get("task").and_then(Value::as_str)
                    && id.len() <= MAX_RESOURCE_SEGMENT_BYTES
                {
                    let uri = short_ref_to_uri(id);
                    if !resource_uri_admitted(&uri) {
                        continue;
                    }
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
                if let Some(plan_ref) = plan.get("plan_ref").and_then(Value::as_str)
                    && plan_ref.len() <= MAX_RESOURCE_SEGMENT_BYTES
                {
                    let uri = short_ref_to_uri(plan_ref);
                    if !resource_uri_admitted(&uri) {
                        continue;
                    }
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
                if let Some(id) = pty.get("pty_id").and_then(Value::as_str)
                    && id.len() <= MAX_RESOURCE_SEGMENT_BYTES
                {
                    let uri = short_ref_to_uri(id);
                    if !resource_uri_admitted(&uri) {
                        continue;
                    }
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

    /// `resources/read` (site/content/internals/kernel-protocol.md): dispatch a `shoal://` URI to the
    /// kernel and return `structuredContent` (the `$`-tagged / elided value).
    /// So an agent following an elided value's ref never hand-translates to
    /// `shoal_get`.
    pub(crate) fn resources_read(&mut self, params: Value) -> Result<Value, String> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or("resources/read requires uri")?;
        let parsed = ParsedUri::parse(uri)?;
        // Session state views. `cwd` is served from the cached attach result (no
        // round-trip); `env`/`reef` read the session evaluator live via a
        // dedicated kernel method (fresh, so in-session `cd`/env-writes/reef
        // locks are reflected). Env is names-only unless granted (site/content/internals/kernel-protocol.md) — the
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
                _ => return Err("unsupported session view".into()),
            };
            return Ok(value_read_result(uri, value));
        }
        // `shoal://task/{id}/out`: the task's captured output — the read side of
        // the subscription (site/content/internals/kernel-protocol.md). A kernel task captures the *whole* outcome at
        // completion (tasks do not expose a live stream cursor), so this returns the full
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
                return Ok(value_read_result(uri, record));
            };
            let value = self.call_kernel(
                "value.get",
                json!({
                    "ref": result_ref,
                    "path": parsed.query.get("path"),
                    "slice": optional_slice(&parsed.query)?,
                    "format": optional_format(&parsed.query)?,
                    "elide": optional_elide(&parsed.query)?,
                }),
            )?;
            return Ok(value_read_result(uri, value));
        }
        let (method, kparams) = parsed.to_kernel_call()?;
        let value = self.call_kernel(method, kparams)?;
        Ok(value_read_result(uri, value))
    }

    /// One kernel JSON-RPC call with the resource layer's error convention: a
    /// kernel-side `error` surfaces as `kernel error: …`, transport failures as
    /// their own display. Shared by every `resources/read` path.
    fn call_kernel(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match self.kernel.call(method, params) {
            Ok(v) => Ok(v),
            Err(BridgeError::Kernel(error)) => Err(safe_kernel_error(&error, "resource")),
            Err(_) => Err("kernel transport failed while reading resource".into()),
        }
    }

    /// `resources/subscribe` (site/content/internals/kernel-protocol.md): map a `shoal://events/{ch}` or
    /// `shoal://task/{id}/out` URI to a kernel channel and forward pushes as
    /// `notifications/resources/updated`. One facade-owned background
    /// connection multiplexes every subscribed channel, separate from the
    /// ordinary request/response transport.
    pub(crate) fn resources_subscribe(&mut self, params: Value) -> Result<Value, String> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or("resources/subscribe requires uri")?;
        let parsed = ParsedUri::parse(uri)?;
        if self
            .subscription_hub
            .as_ref()
            .is_some_and(crate::subscriptions::SubscriptionHub::is_finished)
        {
            self.subscription_hub.take();
            self.subscriptions.clear();
        }
        let duplicate = self.subscriptions.contains_key(uri);
        if !crate::subscription_admission(self.subscriptions.len(), uri, duplicate)? {
            return Ok(json!({}));
        }
        let channel = parsed
            .event_channel()
            .ok_or("only shoal://events/{ch} and shoal://task/{id}[/out] are subscribable")?;
        let uri = uri.to_string();
        if self.subscription_hub.is_none() {
            self.subscription_hub = Some(crate::subscriptions::SubscriptionHub::connect(
                &self.config,
            )?);
        }
        let Some(hub) = self.subscription_hub.as_ref() else {
            return Err("kernel subscription worker initialization failed".into());
        };
        if let Err(error) = hub.add(uri.clone(), channel.clone()) {
            if self.subscriptions.is_empty() {
                self.subscription_hub.take();
            }
            return Err(error);
        }
        self.subscriptions.insert(uri, channel);
        Ok(json!({}))
    }

    pub(crate) fn resources_unsubscribe(&mut self, params: Value) -> Result<Value, String> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or("resources/unsubscribe requires uri")?;
        let channel = ParsedUri::parse(uri)?
            .event_channel()
            .ok_or("only subscribable resource URIs may be unsubscribed")?;
        if self
            .subscription_hub
            .as_ref()
            .is_some_and(crate::subscriptions::SubscriptionHub::is_finished)
        {
            self.subscription_hub.take();
            self.subscriptions.clear();
            return Ok(json!({}));
        }
        if let Some(stored_channel) = self.subscriptions.get(uri) {
            debug_assert_eq!(stored_channel, &channel);
            let hub = self
                .subscription_hub
                .as_ref()
                .ok_or("kernel subscription worker is unavailable")?;
            hub.remove(uri.to_string(), stored_channel.clone())?;
            self.subscriptions.remove(uri);
        }
        Ok(json!({}))
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
    let value = if value_within_admission(&value, MAX_RESOURCE_RESULT_BYTES) {
        value
    } else {
        json!({"$":"elided","reason":"kernel result exceeded MCP resource-content limit"})
    };
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

/// `resources/templates/list` (site/content/internals/kernel-protocol.md): the query-parameterized
/// forms an agent can instantiate.
pub(crate) fn resource_templates() -> Value {
    json!({"resourceTemplates":[
        {"uriTemplate":"shoal://out/{n}{?path,slice,format}","name":"transcript-value","description":"A transcript value, drillable by field-path/slice","mimeType":"application/json"},
        {"uriTemplate":"shoal://val/{hash}{?offset,length}","name":"content-value","description":"An immutable content-addressed value, retrieved in bounded byte pages","mimeType":"application/json"},
        {"uriTemplate":"shoal://task/{id}","name":"task","description":"A background task record","mimeType":"application/json"},
        {"uriTemplate":"shoal://task/{id}/out{?path,slice,format}","name":"task-output","description":"A task's captured output, drillable by field-path/slice","mimeType":"application/json"},
        {"uriTemplate":"shoal://plan/{ref}","name":"plan","description":"A derived plan: effects, reversibility, verdict","mimeType":"application/json"},
        {"uriTemplate":"shoal://session/{view}","name":"session-view","description":"A session state view: cwd | env | reef","mimeType":"application/json"},
        {"uriTemplate":"shoal://pty/{id}","name":"pty-screen","description":"An open interactive PTY session's rendered screen (same shape as pty.read)","mimeType":"application/json"},
        {"uriTemplate":"shoal://journal{?since,until,head,principal,ok,effects,limit}","name":"journal","description":"The structured execution journal","mimeType":"application/json"},
        {"uriTemplate":"shoal://events/{channel}{?since,limit}","name":"events","description":"A cursor-read event channel","mimeType":"application/json"}
    ]})
}

/// A parsed `shoal://` resource URI (site/content/internals/kernel-protocol.md).
struct ParsedUri {
    root: String,
    segments: Vec<String>,
    query: HashMap<String, String>,
}

impl ParsedUri {
    fn parse(uri: &str) -> Result<Self, String> {
        if uri.len() > MAX_RESOURCE_URI_BYTES {
            return Err("resource URI exceeds the 4 KiB admission limit".into());
        }
        if uri.contains('#') || uri.contains('\0') {
            return Err("resource URI contains a forbidden delimiter".into());
        }
        let rest = uri
            .strip_prefix("shoal://")
            .ok_or("resource URI must use the shoal:// scheme")?;
        let (path, query_str) = match rest.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (rest, None),
        };
        if path.is_empty() {
            return Err("resource URI has an empty path".into());
        }
        let raw_segments = path.split('/').collect::<Vec<_>>();
        if raw_segments.len() > MAX_RESOURCE_SEGMENTS || raw_segments.iter().any(|s| s.is_empty()) {
            return Err("resource URI has an invalid path shape".into());
        }
        let mut segments = Vec::with_capacity(raw_segments.len());
        for segment in raw_segments {
            let decoded = percent_decode(segment, MAX_RESOURCE_SEGMENT_BYTES, "path segment")?;
            if decoded.contains(['/', '?', '#', '\0']) {
                return Err("resource URI path segment contains a forbidden delimiter".into());
            }
            segments.push(decoded);
        }
        let root = segments.remove(0);
        let mut query = HashMap::new();
        if let Some(q) = query_str {
            if q.is_empty() {
                return Err("resource URI has an empty query".into());
            }
            let pairs = q.split('&').collect::<Vec<_>>();
            if pairs.len() > MAX_RESOURCE_QUERY_PAIRS || pairs.iter().any(|pair| pair.is_empty()) {
                return Err("resource URI has too many or empty query pairs".into());
            }
            for pair in pairs {
                let (raw_key, raw_value) = pair
                    .split_once('=')
                    .ok_or("resource URI query pair must contain '='")?;
                let key = percent_decode(raw_key, MAX_RESOURCE_QUERY_KEY_BYTES, "query key")?;
                let value =
                    percent_decode(raw_value, MAX_RESOURCE_QUERY_VALUE_BYTES, "query value")?;
                if key.is_empty() || query.insert(key, value).is_some() {
                    return Err("resource URI has an empty or duplicate query key".into());
                }
            }
        }
        let parsed = Self {
            root,
            segments,
            query,
        };
        parsed.validate_schema()?;
        Ok(parsed)
    }

    fn validate_schema(&self) -> Result<(), String> {
        let (shape_ok, allowed): (bool, &[&str]) = match self.root.as_str() {
            "out" => (
                self.segments.len() == 1,
                &["path", "slice", "format", "elide"],
            ),
            "val" => (self.segments.len() == 1, &["offset", "length"]),
            "plan" => (self.segments.len() == 1, &[]),
            "task" if self.segments.len() == 1 => (true, &[]),
            "task" if self.segments.len() == 2 && self.segments[1] == "out" => {
                (true, &["path", "slice", "format", "elide"])
            }
            "task" => (false, &[]),
            "pty" => (self.segments.len() <= 1, &[]),
            "jobs" => (self.segments.is_empty(), &[]),
            "journal" => (
                self.segments.is_empty(),
                &[
                    "since",
                    "until",
                    "head",
                    "principal",
                    "ok",
                    "effects",
                    "limit",
                ],
            ),
            "events" => (self.segments.len() == 1, &["since", "limit"]),
            "session" => (
                self.segments.len() == 1
                    && matches!(self.segments[0].as_str(), "cwd" | "env" | "reef"),
                &[],
            ),
            _ => return Err("unsupported resource root".into()),
        };
        if !shape_ok {
            return Err("resource URI path does not match its resource schema".into());
        }
        if self
            .query
            .keys()
            .any(|key| !allowed.contains(&key.as_str()))
        {
            return Err("resource URI contains an unsupported query parameter".into());
        }
        Ok(())
    }

    /// The kernel JSON-RPC call this resource read maps to.
    fn to_kernel_call(&self) -> Result<(&'static str, Value), String> {
        match self.root.as_str() {
            "out" => {
                let n = self
                    .segments
                    .first()
                    .ok_or("shoal://out/{n} needs an index")?;
                let slice = optional_slice(&self.query)?;
                Ok((
                    "value.get",
                    json!({
                        "ref": format!("out:{n}"),
                        "path": self.query.get("path"),
                        "slice": slice,
                        "elide": optional_elide(&self.query)?,
                        "format": optional_format(&self.query)?,
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
                // (site/content/internals/kernel-protocol.md): the kernel's CAS keys on the raw hex, so
                // strip the `blake3:` algorithm prefix before `blob.get`.
                let hash = hash.strip_prefix("blake3:").unwrap_or(hash);
                Ok((
                    "blob.get",
                    json!({
                        "hash": hash,
                        "offset": optional_number::<u64>(&self.query, "offset")?,
                        "length": optional_number::<u64>(&self.query, "length")?,
                    }),
                ))
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
                    "since": optional_number::<i64>(&self.query, "since")?,
                    "until": optional_number::<i64>(&self.query, "until")?,
                    "head": self.query.get("head"),
                    "principal": self.query.get("principal"),
                    "ok": optional_number::<bool>(&self.query, "ok")?,
                    "effects": optional_effects(&self.query)?,
                    // Absent `limit` travels as `null` (not `0`): the kernel
                    // reads a missing limit as "default page" and an explicit
                    // `0` as "zero rows" (site/content/internals/kernel-rpc-reference.md). Sending
                    // `0` here would silently return an empty journal.
                    "limit": optional_number::<usize>(&self.query, "limit")?,
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
                        "since": optional_number::<u64>(&self.query, "since")?,
                        "limit": optional_number::<usize>(&self.query, "limit")?,
                    }),
                ))
            }
            _ => Err("unsupported resource root".into()),
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

fn optional_number<T>(query: &HashMap<String, String>, key: &str) -> Result<Option<T>, String>
where
    T: std::str::FromStr,
{
    query
        .get(key)
        .map(|value| {
            value
                .parse::<T>()
                .map_err(|_| format!("resource query parameter {key} is invalid"))
        })
        .transpose()
}

fn optional_slice(query: &HashMap<String, String>) -> Result<Option<Value>, String> {
    let Some(value) = query.get("slice") else {
        return Ok(None);
    };
    let (start, end) = value
        .split_once("..")
        .ok_or("resource slice must be START..END")?;
    let start = start
        .parse::<usize>()
        .map_err(|_| "resource slice start is invalid")?;
    let end = end
        .parse::<usize>()
        .map_err(|_| "resource slice end is invalid")?;
    if start > end {
        return Err("resource slice start exceeds its end".into());
    }
    Ok(Some(json!([start, end])))
}

fn optional_elide(query: &HashMap<String, String>) -> Result<Option<Value>, String> {
    query
        .get("elide")
        .map(|value| {
            let parsed: Value = serde_json::from_str(value)
                .map_err(|_| "resource elide parameter is not valid JSON")?;
            if !parsed.is_object() {
                return Err("resource elide parameter must be an object".into());
            }
            Ok(parsed)
        })
        .transpose()
}

fn optional_format(query: &HashMap<String, String>) -> Result<Option<&String>, String> {
    match query.get("format") {
        Some(value) if matches!(value.as_str(), "render" | "raw") => Ok(Some(value)),
        Some(_) => Err("resource format must be render or raw".into()),
        None => Ok(None),
    }
}

fn optional_effects(query: &HashMap<String, String>) -> Result<Option<Vec<String>>, String> {
    let Some(value) = query.get("effects") else {
        return Ok(None);
    };
    let effects = value.split(',').collect::<Vec<_>>();
    if effects.len() > 64
        || effects
            .iter()
            .any(|effect| effect.is_empty() || effect.len() > 128)
    {
        return Err("resource effects filter is invalid".into());
    }
    let mut unique = HashSet::with_capacity(effects.len());
    if effects.iter().any(|effect| !unique.insert(*effect)) {
        return Err("resource effects filter contains duplicates".into());
    }
    Ok(Some(effects.into_iter().map(String::from).collect()))
}

fn safe_kernel_error(error: &Value, operation: &str) -> String {
    match error.get("code").and_then(Value::as_i64) {
        Some(code) => format!("kernel rejected {operation} request (code {code})"),
        None => format!("kernel rejected {operation} request"),
    }
}

/// Strict percent decoding with a decoded-byte wall. Malformed escapes and
/// non-UTF-8 octets are rejected rather than normalized into an ambiguous URI.
fn percent_decode(s: &str, max_bytes: usize, component: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len().min(max_bytes));
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(format!(
                    "resource URI {component} has malformed percent encoding"
                ));
            }
            let high = hex_value(bytes[i + 1]).ok_or_else(|| {
                format!("resource URI {component} has malformed percent encoding")
            })?;
            let low = hex_value(bytes[i + 2]).ok_or_else(|| {
                format!("resource URI {component} has malformed percent encoding")
            })?;
            out.push((high << 4) | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
        if out.len() > max_bytes {
            return Err(format!("resource URI {component} exceeds its byte limit"));
        }
    }
    let decoded = String::from_utf8(out)
        .map_err(|_| format!("resource URI {component} is not valid UTF-8"))?;
    if decoded.chars().any(char::is_control) {
        return Err(format!(
            "resource URI {component} contains a control character"
        ));
    }
    Ok(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

        let val_page =
            ParsedUri::parse("shoal://val/abc123?offset=8192&length=18446744073709551615").unwrap();
        let (method, params) = val_page.to_kernel_call().unwrap();
        assert_eq!(method, "blob.get");
        assert_eq!(params["offset"], 8192);
        assert_eq!(params["length"], u64::MAX);

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

    #[test]
    fn rejects_hostile_resource_uris_before_dispatch() {
        let hostile = "SECRETPATH".repeat(MAX_RESOURCE_URI_BYTES);
        let error = ParsedUri::parse(&format!("shoal://out/{hostile}"))
            .err()
            .expect("oversized URI must fail");
        assert!(!error.contains("SECRETPATH"));

        for uri in [
            "shoal://out/1?path=a&path=b",
            "shoal://out/1?path=a&%70ath=b",
            "shoal://out/1?unknown=x",
            "shoal://out/1?path=%FF",
            "shoal://events/user%0Athread",
            "shoal://out/%2F",
            "shoal://out//1",
            "shoal://out/1/extra",
            "shoal://jobs?limit=1",
            "shoal://session/token",
            "shoal://events/channel?since=not-a-number",
            "shoal://events/channel?since=18446744073709551616",
            "shoal://journal?ok=TRUE",
            "shoal://journal?effects=fs.read,fs.read",
        ] {
            let parsed = ParsedUri::parse(uri);
            if let Ok(parsed) = parsed {
                assert!(
                    parsed.to_kernel_call().is_err(),
                    "hostile URI unexpectedly dispatched: {uri}"
                );
            }
        }

        let pair_flood = format!(
            "shoal://journal?{}",
            (0..=MAX_RESOURCE_QUERY_PAIRS)
                .map(|index| format!("limit{index}=1"))
                .collect::<Vec<_>>()
                .join("&")
        );
        assert!(ParsedUri::parse(&pair_flood).is_err());
    }

    #[test]
    fn resource_numeric_extremes_remain_explicit_and_clampable_by_kernel() {
        let events = ParsedUri::parse(&format!(
            "shoal://events/user.page?since={}&limit={}",
            u64::MAX,
            usize::MAX
        ))
        .unwrap();
        let (_, params) = events.to_kernel_call().unwrap();
        assert_eq!(params["since"], u64::MAX);
        assert_eq!(params["limit"], usize::MAX);

        let journal = ParsedUri::parse(&format!(
            "shoal://journal?since={}&until={}&limit={}",
            i64::MIN,
            i64::MAX,
            usize::MAX
        ))
        .unwrap();
        let (_, params) = journal.to_kernel_call().unwrap();
        assert_eq!(params["since"], i64::MIN);
        assert_eq!(params["until"], i64::MAX);
        assert_eq!(params["limit"], usize::MAX);
    }

    #[test]
    fn advertised_resource_templates_parse_as_their_ordinary_forms() {
        for uri in [
            "shoal://out/1?path=.name&slice=0..1&format=render",
            "shoal://val/blake3:abc?offset=0&length=8192",
            "shoal://task/1",
            "shoal://task/1/out?path=.value",
            "shoal://plan/abc",
            "shoal://session/cwd",
            "shoal://pty/1",
            "shoal://journal?since=-1&ok=true&limit=25",
            "shoal://events/user.page?since=0&limit=25",
        ] {
            let parsed = ParsedUri::parse(uri).unwrap();
            if parsed.root != "session" && !(parsed.root == "task" && parsed.segments.len() == 2) {
                parsed.to_kernel_call().unwrap();
            }
        }
        assert_eq!(
            resource_templates()["resourceTemplates"]
                .as_array()
                .unwrap()
                .len(),
            9
        );
    }

    #[test]
    fn raw_resource_structured_content_stays_below_context_wall() {
        let encoded_len = shoal_proto::RAW_PAGE_MAX_BYTES.div_ceil(3) * 4;
        let value = json!({
            "hash": "a".repeat(64),
            "encoding": "base64",
            "raw_base64": "q".repeat(encoded_len),
            "page": {
                "total_len": shoal_proto::RAW_PAGE_MAX_BYTES * 4,
                "offset": 0,
                "returned_len": shoal_proto::RAW_PAGE_MAX_BYTES,
                "next_offset": shoal_proto::RAW_PAGE_MAX_BYTES,
                "done": false,
                "truncated": true,
                "unit": "byte",
            },
        });
        let result = value_read_result("shoal://val/a", value);
        assert!(
            serde_json::to_vec(&result["structuredContent"])
                .unwrap()
                .len()
                < 64 * 1024
        );
        assert!(result["contents"][0]["text"].as_str().unwrap().len() < 64 * 1024);

        let oversized = value_read_result(
            "shoal://events/user.page",
            Value::Array(vec![Value::Null; 257]),
        );
        assert_eq!(oversized["structuredContent"]["$"], "elided");
    }
}
