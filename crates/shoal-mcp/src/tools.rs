//! `tools/*` handlers: the `shoal_*` tool schemas, the MCP→kernel method
//! mapping, and the bounded `tools/call` result shape (site/content/internals/kernel-protocol.md).

use crate::{BridgeError, Facade, short_ref_to_uri};
use serde_json::{Value, json};
use std::collections::HashSet;

const MAX_TOOL_NAME_BYTES: usize = 64;
const MAX_TOOL_ARGUMENT_BYTES: usize = 4 * 1024 * 1024 + 64 * 1024;
const MAX_TOOL_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOOL_IDENTIFIER_BYTES: usize = 4 * 1024;
const MAX_TOOL_COLLECTION_ITEMS: usize = 256;
const MAX_TOOL_RESULT_BYTES: usize = 8 * 1024 * 1024;

impl Facade {
    pub(crate) fn tools_call(&mut self, mut params: Value) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or("tools/call requires name")?;
        admit_tool_name(name)?;
        let name = name.to_string();
        let args = params
            .get_mut("arguments")
            .map(Value::take)
            .unwrap_or_else(|| json!({}));
        admit_value_size(&args, MAX_TOOL_ARGUMENT_BYTES, "tool arguments")?;
        validate_tool_arguments(&name, &args)?;
        let (method, kparams) = map_tool(&name, args)?;
        match self.kernel.call(method, kparams) {
            Ok(result) => Ok(tool_result(result, false)),
            Err(BridgeError::Kernel(error)) => Ok(tool_result(safe_tool_error(&error), true)),
            Err(_) => Err("kernel transport failed while calling tool".into()),
        }
    }
}

fn admit_tool_name(name: &str) -> Result<(), String> {
    if name.len() > MAX_TOOL_NAME_BYTES {
        return Err("tool name exceeds the admission limit".into());
    }
    if !matches!(
        name,
        "shoal_exec"
            | "shoal_plan"
            | "shoal_apply"
            | "shoal_get"
            | "shoal_stream_pull"
            | "shoal_stream_close"
            | "shoal_journal"
            | "shoal_cancel"
            | "shoal_cap_request"
            | "shoal_pty_open"
            | "shoal_pty_send"
            | "shoal_pty_read"
            | "shoal_pty_resize"
            | "shoal_pty_close"
            | "shoal_pty_list"
    ) {
        return Err("unknown tool".into());
    }
    Ok(())
}

fn admit_value_size(value: &Value, limit: usize, label: &str) -> Result<(), String> {
    struct LimitWriter {
        remaining: usize,
    }

    impl std::io::Write for LimitWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                return Err(std::io::Error::other("JSON byte limit exceeded"));
            }
            self.remaining -= bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn visit(value: &Value, remaining: &mut usize) -> bool {
        let cost = match value {
            Value::Null => 4,
            Value::Bool(_) => 5,
            Value::Number(number) => number.to_string().len(),
            Value::String(string) => string.len().saturating_add(2),
            Value::Array(values) => {
                if values.len() > MAX_TOOL_COLLECTION_ITEMS || !take(remaining, 2 + values.len()) {
                    return false;
                }
                return values.iter().all(|value| visit(value, remaining));
            }
            Value::Object(values) => {
                if values.len() > MAX_TOOL_COLLECTION_ITEMS || !take(remaining, 2 + values.len()) {
                    return false;
                }
                for (key, value) in values {
                    if !take(remaining, key.len().saturating_add(3)) || !visit(value, remaining) {
                        return false;
                    }
                }
                return true;
            }
        };
        take(remaining, cost)
    }
    fn take(remaining: &mut usize, cost: usize) -> bool {
        if let Some(next) = remaining.checked_sub(cost) {
            *remaining = next;
            true
        } else {
            false
        }
    }

    let mut remaining = limit;
    let mut writer = LimitWriter { remaining: limit };
    if visit(value, &mut remaining) && serde_json::to_writer(&mut writer, value).is_ok() {
        Ok(())
    } else {
        Err(format!("{label} exceed the admission limit"))
    }
}

pub(crate) fn value_within_admission(value: &Value, limit: usize) -> bool {
    admit_value_size(value, limit, "value").is_ok()
}

fn validate_tool_arguments(name: &str, args: &Value) -> Result<(), String> {
    let object = args.as_object().ok_or("tool arguments must be an object")?;
    let allowed: &[&str] = match name {
        "shoal_exec" => &[
            "src",
            "mode",
            "position",
            "background",
            "timeout_ms",
            "deadline_ms",
            "elide",
        ],
        "shoal_plan" => &["src"],
        "shoal_apply" => &["plan_ref"],
        "shoal_get" => &["ref", "path", "slice", "elide"],
        "shoal_stream_pull" => &["cursor", "limit", "wait_ms", "deadline_ms", "elide"],
        "shoal_stream_close" => &["cursor"],
        "shoal_journal" => &[
            "since",
            "until",
            "principal",
            "ok",
            "effects",
            "head",
            "limit",
        ],
        "shoal_cancel" => &["task"],
        "shoal_cap_request" => &["plan_ref", "effects"],
        "shoal_pty_open" => &["cmd", "args", "cols", "rows", "env"],
        "shoal_pty_send" => &["pty_id", "input"],
        "shoal_pty_read" | "shoal_pty_close" => &["pty_id"],
        "shoal_pty_resize" => &["pty_id", "cols", "rows"],
        "shoal_pty_list" => &[],
        _ => return Err("unknown tool".into()),
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("tool arguments contain an unsupported field".into());
    }

    match name {
        "shoal_exec" => {
            bounded_string(object, "src", MAX_TOOL_SOURCE_BYTES)?;
            optional_enum(object, "mode", &["run", "plan"])?;
            optional_enum(object, "position", &["stmt", "value"])?;
            optional_bool(object, "background")?;
            optional_u64(object, "timeout_ms", 1, u64::MAX, false)?;
            optional_u64(object, "deadline_ms", 1, u64::MAX, false)?;
            optional_elide(object.get("elide"))?;
        }
        "shoal_plan" => bounded_string(object, "src", MAX_TOOL_SOURCE_BYTES)?,
        "shoal_apply" | "shoal_cap_request" => {
            bounded_string(object, "plan_ref", MAX_TOOL_IDENTIFIER_BYTES)?
        }
        "shoal_get" => {
            bounded_string(object, "ref", MAX_TOOL_IDENTIFIER_BYTES)?;
            optional_bounded_string(object, "path", MAX_TOOL_IDENTIFIER_BYTES)?;
            optional_slice(object.get("slice"))?;
            optional_elide(object.get("elide"))?;
        }
        "shoal_cancel" => bounded_string(object, "task", MAX_TOOL_IDENTIFIER_BYTES)?,
        "shoal_pty_open" => {
            bounded_string(object, "cmd", MAX_TOOL_IDENTIFIER_BYTES)?;
            validate_string_array(
                object.get("args"),
                MAX_TOOL_COLLECTION_ITEMS,
                MAX_TOOL_IDENTIFIER_BYTES,
            )?;
            validate_string_map(object.get("env"), MAX_TOOL_COLLECTION_ITEMS)?;
            optional_u64(object, "cols", 1, 1000, false)?;
            optional_u64(object, "rows", 1, 1000, false)?;
        }
        "shoal_pty_send" => {
            bounded_string(object, "pty_id", MAX_TOOL_IDENTIFIER_BYTES)?;
            if !object.contains_key("input") {
                return Err("missing pty input".into());
            }
        }
        "shoal_pty_read" | "shoal_pty_close" => {
            bounded_string(object, "pty_id", MAX_TOOL_IDENTIFIER_BYTES)?
        }
        "shoal_pty_resize" => {
            bounded_string(object, "pty_id", MAX_TOOL_IDENTIFIER_BYTES)?;
            optional_u64(object, "cols", 1, 1000, true)?;
            optional_u64(object, "rows", 1, 1000, true)?;
        }
        "shoal_journal" => {
            optional_bounded_string(object, "principal", MAX_TOOL_IDENTIFIER_BYTES)?;
            optional_bounded_string(object, "head", MAX_TOOL_IDENTIFIER_BYTES)?;
            validate_string_array(object.get("effects"), 64, 128)?;
            optional_i64(object, "since")?;
            optional_i64(object, "until")?;
            optional_bool(object, "ok")?;
            optional_u64(object, "limit", 1, u64::MAX, false)?;
        }
        "shoal_stream_pull" => {
            validate_cursor(object.get("cursor"))?;
            optional_u64(object, "limit", 1, 64, false)?;
            optional_u64(object, "wait_ms", 0, 1000, false)?;
            optional_u64(object, "deadline_ms", 1, 30_000, false)?;
            optional_elide(object.get("elide"))?;
        }
        "shoal_stream_close" => validate_cursor(object.get("cursor"))?,
        "shoal_pty_list" => {}
        _ => {}
    }
    if name == "shoal_cap_request" {
        validate_string_array(object.get("effects"), 128, 128)?;
    }
    Ok(())
}

fn bounded_string(
    object: &serde_json::Map<String, Value>,
    field: &str,
    max_bytes: usize,
) -> Result<(), String> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string argument {field:?}"))?;
    if value.len() > max_bytes {
        return Err(format!("string argument {field:?} exceeds its byte limit"));
    }
    Ok(())
}

fn optional_bounded_string(
    object: &serde_json::Map<String, Value>,
    field: &str,
    max_bytes: usize,
) -> Result<(), String> {
    match object.get(field) {
        Some(Value::String(value)) if value.len() <= max_bytes => Ok(()),
        Some(Value::String(_)) => Err(format!("string argument {field:?} exceeds its byte limit")),
        Some(_) => Err(format!("argument {field:?} must be a string")),
        None => Ok(()),
    }
}

fn validate_string_array(
    value: Option<&Value>,
    max_items: usize,
    max_bytes: usize,
) -> Result<(), String> {
    let Some(value) = value else { return Ok(()) };
    let values = value
        .as_array()
        .ok_or("tool string list must be an array")?;
    if values.len() > max_items
        || values
            .iter()
            .any(|value| value.as_str().is_none_or(|string| string.len() > max_bytes))
    {
        return Err("tool string list exceeds its item or byte limit".into());
    }
    Ok(())
}

fn optional_enum(
    object: &serde_json::Map<String, Value>,
    field: &str,
    allowed: &[&str],
) -> Result<(), String> {
    match object.get(field) {
        Some(Value::String(value)) if allowed.contains(&value.as_str()) => Ok(()),
        Some(_) => Err(format!("argument {field:?} is invalid")),
        None => Ok(()),
    }
}

fn optional_bool(object: &serde_json::Map<String, Value>, field: &str) -> Result<(), String> {
    match object.get(field) {
        Some(Value::Bool(_)) | None => Ok(()),
        Some(_) => Err(format!("argument {field:?} must be a boolean")),
    }
}

fn optional_u64(
    object: &serde_json::Map<String, Value>,
    field: &str,
    min: u64,
    max: u64,
    required: bool,
) -> Result<(), String> {
    match object.get(field).and_then(Value::as_u64) {
        Some(value) if (min..=max).contains(&value) => Ok(()),
        Some(_) => Err(format!("argument {field:?} is outside its numeric range")),
        None if object.contains_key(field) => Err(format!("argument {field:?} must be an integer")),
        None if required => Err(format!("missing integer argument {field:?}")),
        None => Ok(()),
    }
}

fn optional_i64(object: &serde_json::Map<String, Value>, field: &str) -> Result<(), String> {
    match object.get(field) {
        Some(value) if value.as_i64().is_some() => Ok(()),
        Some(_) => Err(format!("argument {field:?} must be a signed integer")),
        None => Ok(()),
    }
}

fn optional_elide(value: Option<&Value>) -> Result<(), String> {
    let Some(value) = value else { return Ok(()) };
    let object = value
        .as_object()
        .ok_or("elide argument must be an object")?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "max_bytes" | "max_rows" | "max_items"))
    {
        return Err("elide argument contains an unsupported field".into());
    }
    for field in ["max_bytes", "max_rows", "max_items"] {
        optional_u64(object, field, 1, u64::MAX, false)?;
    }
    Ok(())
}

fn optional_slice(value: Option<&Value>) -> Result<(), String> {
    let Some(value) = value else { return Ok(()) };
    let values = value
        .as_array()
        .filter(|values| values.len() == 2)
        .ok_or("slice argument must contain exactly two integers")?;
    let start = values[0]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or("slice start is invalid")?;
    let end = values[1]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or("slice end is invalid")?;
    if start > end {
        return Err("slice start exceeds its end".into());
    }
    Ok(())
}

fn validate_string_map(value: Option<&Value>, max_items: usize) -> Result<(), String> {
    let Some(value) = value else { return Ok(()) };
    let values = value
        .as_object()
        .ok_or("tool environment must be an object")?;
    if values.len() > max_items
        || values.iter().any(|(key, value)| {
            key.len() > 256
                || value
                    .as_str()
                    .is_none_or(|string| string.len() > MAX_TOOL_IDENTIFIER_BYTES)
        })
    {
        return Err("tool environment exceeds its item or byte limit".into());
    }
    Ok(())
}

fn validate_cursor(value: Option<&Value>) -> Result<(), String> {
    let cursor = value
        .and_then(Value::as_object)
        .ok_or("missing cursor object")?;
    let allowed = HashSet::from(["ref", "path"]);
    if cursor.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err("cursor contains an unsupported field".into());
    }
    bounded_string(cursor, "ref", MAX_TOOL_IDENTIFIER_BYTES)?;
    optional_bounded_string(cursor, "path", MAX_TOOL_IDENTIFIER_BYTES)
}

fn safe_tool_error(error: &Value) -> Value {
    match error.get("code").and_then(Value::as_i64) {
        Some(code) => json!({"code":code,"message":"kernel rejected tool request"}),
        None => json!({"message":"kernel rejected tool request"}),
    }
}

fn map_tool(name: &str, args: Value) -> Result<(&'static str, Value), String> {
    let object = args.as_object().ok_or("tool arguments must be an object")?;
    Ok(match name {
        // site/content/internals/kernel-protocol.md exec signature: mode/position/background/timeout_ms/deadline_ms/
        // elide are all forwarded (no more silently-dropped params).
        "shoal_exec" => (
            "exec",
            json!({
                "src": required_str(object,"src")?,
                "mode": object.get("mode").and_then(Value::as_str).unwrap_or("run"),
                "position": object.get("position").and_then(Value::as_str).unwrap_or("value"),
                "background": object.get("background").and_then(Value::as_bool).unwrap_or(false),
                "timeout_ms": object.get("timeout_ms"),
                "deadline_ms": object.get("deadline_ms"),
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
        // (site/content/internals/kernel-protocol.md) — previously accepted but dropped.
        "shoal_get" => (
            "value.get",
            json!({
                "ref": required_str(object,"ref")?,
                "path": object.get("path"),
                "slice": object.get("slice"),
                "elide": object.get("elide"),
            }),
        ),
        "shoal_stream_pull" => (
            "stream.pull",
            json!({
                "cursor": object.get("cursor").cloned().ok_or("missing cursor")?,
                "limit": object.get("limit"),
                "wait_ms": object.get("wait_ms"),
                "deadline_ms": object.get("deadline_ms"),
                "elide": object.get("elide"),
            }),
        ),
        "shoal_stream_close" => (
            "stream.close",
            json!({
                "cursor": object.get("cursor").cloned().ok_or("missing cursor")?,
            }),
        ),
        // `until`/`effects`/`ok` are honored kernel-side; forward verbatim.
        "shoal_journal" => ("journal.query", args),
        // Task cancellation (site/content/internals/kernel-protocol.md).
        "shoal_cancel" => ("task.cancel", json!({"task": required_str(object,"task")?})),
        // Escalation path for a plan stuck at `approval_pending` (site/content/internals/language-conformance-contract.md
        // `cap.request`): without this an agent that hits a stricter-than-
        // default leash policy has no MCP-reachable way to move forward.
        // `effects` scopes the grant (site/content/internals/kernel-protocol.md).
        "shoal_cap_request" => (
            "cap.request",
            json!({"plan_ref":required_str(object,"plan_ref")?,"effects":object.get("effects").cloned().unwrap_or_else(||json!([]))}),
        ),
        // Interactive-PTY surface (site/content/internals/kernel-protocol.md): drive a real TUI/REPL
        // over the wire and read back a rendered screen.
        "shoal_pty_open" => (
            "pty.open",
            json!({
                "cmd": required_str(object,"cmd")?,
                "args": object.get("args").cloned().unwrap_or_else(||json!([])),
                "cols": object.get("cols"),
                "rows": object.get("rows"),
                "env": object.get("env").cloned().unwrap_or_else(||json!({})),
            }),
        ),
        "shoal_pty_send" => (
            "pty.send",
            json!({
                "pty_id": required_str(object,"pty_id")?,
                "input": object.get("input").cloned().unwrap_or(Value::Null),
            }),
        ),
        "shoal_pty_read" => (
            "pty.read",
            json!({"pty_id": required_str(object,"pty_id")?}),
        ),
        "shoal_pty_resize" => (
            "pty.resize",
            json!({
                "pty_id": required_str(object,"pty_id")?,
                "cols": object.get("cols"),
                "rows": object.get("rows"),
            }),
        ),
        "shoal_pty_close" => (
            "pty.close",
            json!({"pty_id": required_str(object,"pty_id")?}),
        ),
        // Enumerate this session's open ptys — the discovery verb behind the
        // `shoal://pty` resource root.
        "shoal_pty_list" => ("pty.list", json!({})),
        _ => return Err("unknown tool".into()),
    })
}
fn required_str<'a>(o: &'a serde_json::Map<String, Value>, name: &str) -> Result<&'a str, String> {
    o.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string argument {name:?}"))
}
/// Absolute per-result cap for text/render sent to the agent (see
/// `site/content/internals/kernel-protocol.md`). A misbehaving agent cannot flood its own context: no render or text
/// content ever exceeds this, regardless of the value's size.
const RESULT_TEXT_HARD_CAP: usize = 64 * 1024;

/// Build a `tools/call` result whose context footprint is bounded
/// (site/content/internals/kernel-protocol.md): `structuredContent` carries the kernel's already-
/// elided value; the human-readable `text` content is the render **head**,
/// truncated with a `…(N more lines, fetch via ref)` marker; and a
/// `resource_link` points at the value's ref so the agent can drill in for
/// zero tokens rather than receiving the payload inline.
fn tool_result(value: Value, is_error: bool) -> Value {
    let value = if admit_value_size(&value, MAX_TOOL_RESULT_BYTES, "tool result").is_ok() {
        value
    } else {
        json!({"$":"elided","reason":"kernel result exceeded MCP structured-content limit"})
    };
    // Find the ref/uri this result is addressable by, for the link + marker.
    let uri = value
        .get("uri")
        .and_then(Value::as_str)
        .filter(|uri| uri.len() <= crate::resources::MAX_RESOURCE_URI_BYTES)
        .map(String::from)
        .or_else(|| {
            value
                .get("value")
                .and_then(|v| v.get("uri"))
                .and_then(Value::as_str)
                .filter(|uri| uri.len() <= crate::resources::MAX_RESOURCE_URI_BYTES)
                .map(String::from)
        })
        .or_else(|| {
            value
                .get("ref")
                .and_then(Value::as_str)
                .filter(|short| short.len() <= MAX_TOOL_IDENTIFIER_BYTES)
                .map(short_ref_to_uri)
        })
        .filter(|uri| crate::resources::resource_uri_admitted(uri));
    // Prefer the kernel's human render; fall back to compact JSON. Either way
    // it is bounded — the render string is NOT trusted to be small (a 150-row
    // table renders to many KiB), which is exactly the elision bypass site/content/internals/kernel-protocol.md closes.
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
pub(crate) fn bound_text(text: &str, uri: Option<&str>) -> String {
    if text.len() <= RESULT_TEXT_HARD_CAP {
        return text.to_string();
    }
    let total_lines = text.lines().count();
    let via = match uri.filter(|uri| uri.len() <= 512) {
        Some(uri) => format!("fetch via {uri}"),
        None => "fetch via ref".into(),
    };
    let marker = format!("…({total_lines} more lines, {via})");
    let budget = RESULT_TEXT_HARD_CAP.saturating_sub(marker.len());
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
        let mut end = budget.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        head.push_str(&text[..end]);
    }
    let remaining = total_lines.saturating_sub(kept);
    format!("{head}…({remaining} more lines, {via})")
}

pub fn tools() -> Vec<Value> {
    vec![
        tool(
            "shoal_exec",
            "Execute shoal source and return a stable transcript reference (or a task ref when background/timed-out)",
            json!({"type":"object","properties":{"src":{"type":"string"},"mode":{"enum":["run","plan"]},"position":{"enum":["stmt","value"]},"background":{"type":"boolean"},"timeout_ms":{"type":"integer","minimum":1},"deadline_ms":{"type":"integer","minimum":1},"elide":{"type":"object","properties":{"max_bytes":{"type":"integer"},"max_rows":{"type":"integer"},"max_items":{"type":"integer"}}}},"required":["src"],"additionalProperties":false}),
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
            "shoal_stream_pull",
            "Pull the next bounded batch from a stream cursor returned by shoal_exec or shoal_get. Items are individually addressable transcript values; done and timed_out are explicit.",
            json!({"type":"object","properties":{"cursor":{"type":"object","properties":{"ref":{"type":"string"},"path":{"type":"string"}},"required":["ref"],"additionalProperties":false},"limit":{"type":"integer","minimum":1,"maximum":64},"wait_ms":{"type":"integer","minimum":0,"maximum":1000},"deadline_ms":{"type":"integer","minimum":1,"maximum":30000},"elide":{"type":"object","properties":{"max_bytes":{"type":"integer"},"max_rows":{"type":"integer"},"max_items":{"type":"integer"}}}},"required":["cursor"],"additionalProperties":false}),
        ),
        tool(
            "shoal_stream_close",
            "Close a stream cursor and release its source, pump, and OS resources.",
            json!({"type":"object","properties":{"cursor":{"type":"object","properties":{"ref":{"type":"string"},"path":{"type":"string"}},"required":["ref"],"additionalProperties":false}},"required":["cursor"],"additionalProperties":false}),
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
        tool(
            "shoal_pty_open",
            "Spawn an interactive program (vim, an installer, a REPL, any TUI) on a real terminal and return a pty_id to drive it. Then use shoal_pty_send to type and shoal_pty_read to see the rendered screen. Leash-gated like any spawn.",
            json!({"type":"object","properties":{"cmd":{"type":"string","description":"program to run, e.g. \"vim\", \"python3\", \"sh\""},"args":{"type":"array","items":{"type":"string"}},"cols":{"type":"integer","minimum":1,"maximum":1000},"rows":{"type":"integer","minimum":1,"maximum":1000},"env":{"type":"object","additionalProperties":{"type":"string"}}},"required":["cmd"],"additionalProperties":false}),
        ),
        tool(
            "shoal_pty_send",
            "Send keystrokes to a pty. `input` is a string (typed verbatim), a named key like {\"key\":\"Enter\"}/{\"key\":\"Escape\"}/{\"key\":\"Ctrl-C\"}, or an ARRAY mixing them, e.g. [\"i\",\"hello\",{\"key\":\"Escape\"},\":wq\",{\"key\":\"Enter\"}]. Named keys: Enter, Tab, Escape, Backspace, Delete, Space, Up/Down/Left/Right, Home, End, PageUp/PageDown, F1-F12, Ctrl-<letter>.",
            json!({"type":"object","properties":{"pty_id":{"type":"string"},"input":{"description":"string | {key|text|bytes} object | array of those"}},"required":["pty_id","input"],"additionalProperties":false}),
        ),
        tool(
            "shoal_pty_read",
            "Read a pty's RENDERED screen: `screen` is an array of text rows (bounded by cols×rows), plus cursor {row,col,hidden}, a `changed` bit (did the screen change since your last read), `alive`, and `exit`. Never returns raw escape bytes.",
            json!({"type":"object","properties":{"pty_id":{"type":"string"}},"required":["pty_id"],"additionalProperties":false}),
        ),
        tool(
            "shoal_pty_resize",
            "Resize a pty's terminal window (and its emulator grid)",
            json!({"type":"object","properties":{"pty_id":{"type":"string"},"cols":{"type":"integer","minimum":1,"maximum":1000},"rows":{"type":"integer","minimum":1,"maximum":1000}},"required":["pty_id","cols","rows"],"additionalProperties":false}),
        ),
        tool(
            "shoal_pty_close",
            "Terminate and reap a pty session (no process is left running)",
            json!({"type":"object","properties":{"pty_id":{"type":"string"}},"required":["pty_id"],"additionalProperties":false}),
        ),
        tool(
            "shoal_pty_list",
            "List the OPEN interactive pty sessions for this session: an array of {pty_id, cmd, pid, cols, rows, alive}. Use it to discover ptys you (or a prior turn) opened; then shoal_pty_read a pty_id to see its rendered screen, or read the shoal://pty resource. Only your own session's ptys are visible.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
    ]
}
fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name":name,"description":description,"inputSchema":input_schema})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_all_tools() {
        let names: Vec<String> = tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"shoal_cancel".to_string()));
        assert!(names.contains(&"shoal_exec".to_string()));
        assert!(names.contains(&"shoal_pty_open".to_string()));
        assert!(names.contains(&"shoal_pty_read".to_string()));
        assert!(names.contains(&"shoal_pty_list".to_string()));
        assert_eq!(tools().len(), 15);
        for t in tools() {
            assert_eq!(t["inputSchema"]["type"], "object")
        }
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
            map_tool("shoal_stream_pull", json!({"cursor":{"ref":"out:1"}}))
                .unwrap()
                .0,
            "stream.pull"
        );
        assert_eq!(
            map_tool("shoal_stream_close", json!({"cursor":{"ref":"out:1"}}))
                .unwrap()
                .0,
            "stream.close"
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
        assert_eq!(
            map_tool("shoal_pty_open", json!({"cmd":"cat"})).unwrap().0,
            "pty.open"
        );
        assert_eq!(
            map_tool("shoal_pty_send", json!({"pty_id":"pty:1","input":"x"}))
                .unwrap()
                .0,
            "pty.send"
        );
        assert_eq!(
            map_tool("shoal_pty_read", json!({"pty_id":"pty:1"}))
                .unwrap()
                .0,
            "pty.read"
        );
        assert_eq!(
            map_tool(
                "shoal_pty_resize",
                json!({"pty_id":"pty:1","cols":80,"rows":24})
            )
            .unwrap()
            .0,
            "pty.resize"
        );
        assert_eq!(
            map_tool("shoal_pty_close", json!({"pty_id":"pty:1"}))
                .unwrap()
                .0,
            "pty.close"
        );
        let (method, params) = map_tool("shoal_pty_list", json!({})).unwrap();
        assert_eq!(method, "pty.list");
        assert_eq!(params, json!({}));
    }
    #[test]
    fn cap_request_forwards_plan_ref() {
        let (method, params) = map_tool("shoal_cap_request", json!({"plan_ref":"plan:x"})).unwrap();
        assert_eq!(method, "cap.request");
        assert_eq!(params["plan_ref"], "plan:x");
        assert_eq!(params["effects"], json!([]));
    }

    /// The elision doctrine (site/content/internals/kernel-protocol.md) at the MCP boundary: a huge render/text is
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
    fn tool_admission_rejects_huge_names_identifiers_and_argument_floods_without_echo() {
        let secret_name = "SECRET_TOOL".repeat(100);
        let error = admit_tool_name(&secret_name).unwrap_err();
        assert!(!error.contains("SECRET_TOOL"));
        assert_eq!(
            admit_tool_name("shoal_exec_typo").unwrap_err(),
            "unknown tool"
        );

        let huge_id = "PRIVATE_REF".repeat(MAX_TOOL_IDENTIFIER_BYTES);
        let error = validate_tool_arguments("shoal_get", &json!({"ref": huge_id})).unwrap_err();
        assert!(!error.contains("PRIVATE_REF"));

        let flooded = json!({"src":"1", "unexpected":"x"});
        assert!(validate_tool_arguments("shoal_exec", &flooded).is_err());
        assert!(
            validate_tool_arguments("shoal_exec", &json!({"src":"1", "timeout_ms":u64::MAX}))
                .is_ok()
        );
        assert!(
            validate_tool_arguments("shoal_exec", &json!({"src":"1", "deadline_ms":u64::MAX}))
                .is_ok()
        );
        assert!(
            validate_tool_arguments("shoal_exec", &json!({"src":"1", "timeout_ms":-1})).is_err()
        );
        assert!(
            validate_tool_arguments(
                "shoal_stream_pull",
                &json!({"cursor":{"ref":"out:1"},"limit":65})
            )
            .is_err()
        );
        assert!(
            validate_tool_arguments("shoal_pty_resize", &json!({"pty_id":"pty:1","cols":80}))
                .is_err()
        );
        let wide = Value::Array(vec![Value::Null; MAX_TOOL_COLLECTION_ITEMS + 1]);
        assert!(admit_value_size(&wide, MAX_TOOL_ARGUMENT_BYTES, "tool arguments").is_err());
        let escaped = Value::String("\0".repeat(1024 * 1024));
        assert!(admit_value_size(&escaped, MAX_TOOL_ARGUMENT_BYTES, "tool arguments").is_err());
    }

    #[test]
    fn exec_forwards_wait_and_execution_budgets_separately() {
        let (method, params) = map_tool(
            "shoal_exec",
            json!({"src":"sleep 1s","timeout_ms":5,"deadline_ms":10}),
        )
        .unwrap();
        assert_eq!(method, "exec");
        assert_eq!(params["timeout_ms"], 5);
        assert_eq!(params["deadline_ms"], 10);
    }

    #[test]
    fn tool_admission_preserves_exact_source_limit_and_bounds_collections() {
        let exact = json!({"src":"x".repeat(MAX_TOOL_SOURCE_BYTES)});
        validate_tool_arguments("shoal_exec", &exact).unwrap();
        let oversized = json!({"src":"x".repeat(MAX_TOOL_SOURCE_BYTES + 1)});
        assert!(validate_tool_arguments("shoal_exec", &oversized).is_err());

        let argv = json!({
            "cmd":"echo",
            "args": vec!["x"; MAX_TOOL_COLLECTION_ITEMS],
            "env": {"A":"B"},
        });
        validate_tool_arguments("shoal_pty_open", &argv).unwrap();
    }

    #[test]
    fn tool_errors_and_links_do_not_reflect_kernel_secrets_or_oversized_uris() {
        let error = safe_tool_error(&json!({
            "code": -32000,
            "message": "failed at /private/path with bearer SECRET",
            "data": {"body":"SECRET_BODY"},
        }));
        let encoded = serde_json::to_string(&error).unwrap();
        assert!(!encoded.contains("SECRET"));
        assert!(!encoded.contains("private/path"));

        let result = tool_result(
            json!({"uri":format!("shoal://out/{}", "x".repeat(crate::resources::MAX_RESOURCE_URI_BYTES)), "value":1}),
            false,
        );
        assert_eq!(result["content"].as_array().unwrap().len(), 1);

        let result = tool_result(
            Value::Array(vec![Value::Null; MAX_TOOL_COLLECTION_ITEMS + 1]),
            false,
        );
        assert_eq!(result["structuredContent"]["$"], "elided");
    }

    #[test]
    fn bound_text_is_a_utf8_byte_cap_even_for_one_multibyte_line() {
        let text = "🪸".repeat(RESULT_TEXT_HARD_CAP);
        let bounded = bound_text(&text, Some(&format!("shoal://out/{}", "x".repeat(2048))));
        assert!(bounded.len() <= RESULT_TEXT_HARD_CAP);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.contains("fetch via ref"));
    }
}
