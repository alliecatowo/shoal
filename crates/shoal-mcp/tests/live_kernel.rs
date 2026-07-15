//! Live end-to-end tests: a real in-process `shoal-kernel` on a real Unix
//! socket, driven both through the `shoal-mcp` facade and via raw JSON-RPC.
//!
//! These prove the agent-surface doctrine holds across the whole stack
//! (AGENT-SURFACE §0–§8): the elision rule bounds render/text at the MCP
//! boundary, an elided value's ref is a live resource, and events round-trip
//! on a user channel.

use serde_json::{Value, json};
use shoal_kernel::Kernel;
use shoal_mcp::{Config, Facade};
use shoal_proto::{JSONRPC, Request, Response, write_frame};
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct LiveKernel {
    socket: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl LiveKernel {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("run/kernel.sock");
        let stop = Arc::new(AtomicBool::new(false));
        let kernel = Kernel::new();
        let serve_socket = socket.clone();
        let serve_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            kernel.serve_until(&serve_socket, serve_stop).unwrap();
        });
        // On macOS the socket file can appear (bind) a beat before `listen()`/
        // accept is actually ready, so a bare `exists()` wait races a connect
        // into ECONNREFUSED (OS code 61). Probe with a real connect until the
        // listener accepts before handing the kernel back — this closes the
        // window for every test that connects (facade or raw stream).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if UnixStream::connect(&socket).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "kernel must accept on its socket"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            socket,
            stop,
            handle: Some(handle),
            _dir: dir,
        }
    }
    fn config(&self) -> Config {
        Config {
            socket: self.socket.clone(),
            session: Some("default".into()),
            token: None,
        }
    }
}
impl Drop for LiveKernel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn call_tool(facade: &mut Facade, name: &str, args: Value) -> Value {
    let request = json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":name,"arguments":args}
    });
    facade.handle(&request).expect("tools/call has a response")["result"].clone()
}

fn read_resource(facade: &mut Facade, uri: &str) -> Value {
    let request = json!({
        "jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":uri}
    });
    let response = facade
        .handle(&request)
        .expect("resources/read has a response");
    assert!(
        response.get("error").is_none(),
        "resources/read {uri} errored: {response}"
    );
    response["result"].clone()
}

/// A >100-row table exec, over the real socket through the MCP facade: the
/// structured value elides (§3), the human `text`/render is bounded and never
/// carries the payload (§1 — the elision-bypass fix), and a `resource_link`
/// points at the ref. Then `resources/read` follows that ref and drills into a
/// single row, which is NOT elided.
#[test]
fn mcp_exec_elides_render_and_text_then_resource_read_drills_in() {
    let live = LiveKernel::start();
    // Enough files that the *rendered* table alone exceeds the 64 KiB text cap,
    // so bounding at the MCP boundary is actually exercised (not a no-op).
    let bigdir = live._dir.path().join("bigdir");
    std::fs::create_dir_all(&bigdir).unwrap();
    for i in 0..2000 {
        std::fs::write(bigdir.join(format!("file{i:05}.txt")), b"x").unwrap();
    }
    let mut facade = Facade::connect(&live.config()).unwrap();

    let result = call_tool(
        &mut facade,
        "shoal_exec",
        json!({"src": format!("ls {}", bigdir.display()), "position":"stmt"}),
    );

    // Structured value: the outcome's `.out` table is elided to a ref.
    let out = &result["structuredContent"]["value"]["out"];
    assert_eq!(out["$"], "ref", "a 2000-row table must elide: {out}");
    assert_eq!(out["of"], "table");
    assert_eq!(out["n"], 2000);

    // §1: the human text content is bounded (<= 64 KiB) even though the raw
    // render of 2000 rows is far larger — the render string cannot bypass the
    // wall. And it carries the fetch marker so the agent knows where to look.
    let content = result["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    assert!(
        text.len() <= 64 * 1024,
        "render/text must be bounded, was {} bytes",
        text.len()
    );
    assert!(
        text.contains("more lines, fetch via"),
        "a truncated render must tell the agent how to fetch the rest"
    );

    // §3 bug fix: `structuredContent.render` (the exec result's own render
    // field, distinct from the `content[0].text` derived from it) must be
    // bounded by the SAME hard cap — a 252 KiB ANSI-laden render sitting
    // right next to a properly-elided `value` is exactly the elision bypass
    // this closes.
    let structured_render = result["structuredContent"]["render"].as_str().unwrap();
    assert!(
        structured_render.len() <= 64 * 1024,
        "structuredContent.render must be capped too, was {} bytes",
        structured_render.len()
    );
    assert!(structured_render.contains("more lines, fetch via"));

    // A resource_link points at the value's ref for zero-token drill-in.
    let link = content
        .iter()
        .find(|c| c["type"] == "resource_link")
        .expect("a value result carries a resource_link");
    let uri = link["uri"].as_str().unwrap().to_string();
    assert!(uri.starts_with("shoal://out/"), "link uri: {uri}");

    // Follow the ref: read a single row by field-path. It is small, so it comes
    // back whole (not elided) — the payload is pulled on purpose, structured.
    let drilled = read_resource(&mut facade, &format!("{uri}?path=out[3]"));
    let value = &drilled["structuredContent"];
    assert_ne!(
        value["$"], "ref",
        "a single drilled row must not elide: {value}"
    );
    assert_eq!(value["$"], "record");
    assert!(
        value["v"]["name"].is_object(),
        "drilled row keeps its fields"
    );
}

/// A small exec through the facade returns the value inline (no elision) and
/// resources/read on `shoal://jobs` and `shoal://journal` browse state.
#[test]
fn mcp_small_value_inline_and_state_resources() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();

    let result = call_tool(&mut facade, "shoal_exec", json!({"src":"1 + 2"}));
    assert_eq!(
        result["structuredContent"]["value"],
        json!({"$":"int","v":3})
    );

    // resources/list advertises the stable roots.
    let list = facade
        .handle(&json!({"jsonrpc":"2.0","id":3,"method":"resources/list"}))
        .unwrap();
    let uris: Vec<String> = list["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap().to_string())
        .collect();
    assert!(uris.iter().any(|u| u == "shoal://journal"));

    // The journal resource lists the just-run entry.
    let journal = read_resource(&mut facade, "shoal://journal");
    let entries = journal["structuredContent"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["src"] == "1 + 2"));
}

/// AGENT-SURFACE §4/§5: `shoal_exec {background:true}` must return an events
/// channel of the form `task.{bare id}` (e.g. `task.7`) — NOT
/// `task.{full ref}` (`task.task:7`), which no `events.read`/
/// `resources/subscribe` caller could ever match against the real channel a
/// task's lifecycle events are actually published on.
#[test]
fn mcp_background_exec_events_channel_is_bare_task_id() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();

    let bg = call_tool(
        &mut facade,
        "shoal_exec",
        json!({"src":"sh { sleep 0.05 }","background":true}),
    );
    let structured = &bg["structuredContent"];
    let task_ref = structured["task"]
        .as_str()
        .expect("background exec returns a task ref")
        .to_string();
    let bare_id = task_ref.strip_prefix("task:").expect("task ref is task:N");
    assert_eq!(
        structured["events"],
        format!("task.{bare_id}"),
        "events channel must be task.{{bare id}}, not double-prefixed: {structured}"
    );
}

/// AGENT-SURFACE §4: a task killed via `shoal_cancel` must read back
/// `state:"cancelled"` in `shoal://jobs` — not `"completed"`. The MCP
/// facade's default `position:"value"` captures a signal-killed outcome
/// (`ok:false, signal:"SIGINT"`) as a normal returned value instead of
/// raising it as an RPC error, so the terminal state must be derived from
/// the outcome itself, not just from whether the eval call raised.
#[test]
fn mcp_cancelled_task_reads_back_cancelled_not_completed() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();

    let bg = call_tool(
        &mut facade,
        "shoal_exec",
        json!({"src":"sh { sleep 5 }","background":true}),
    );
    let task_ref = bg["structuredContent"]["task"]
        .as_str()
        .expect("background exec returns a task ref")
        .to_string();

    let cancel = call_tool(&mut facade, "shoal_cancel", json!({"task": task_ref}));
    assert_ne!(
        cancel["isError"], true,
        "cancel request must succeed: {cancel}"
    );

    // Cancellation kills the child asynchronously (SIGINT, then escalating)
    // — poll `shoal://jobs` until the task leaves its transient state
    // instead of assuming the very next read has already settled.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_state = String::new();
    loop {
        let jobs = read_resource(&mut facade, "shoal://jobs");
        let tasks = jobs["structuredContent"].as_array().unwrap().clone();
        if let Some(task) = tasks.iter().find(|t| t["task"] == json!(task_ref)) {
            last_state = task["state"].as_str().unwrap_or_default().to_string();
            if last_state != "running" && last_state != "cancelling" {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "task never reached a terminal state, last seen {last_state:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        last_state, "cancelled",
        "a shoal_cancel'd task must read back cancelled, not completed/failed"
    );
}

/// AGENT-SURFACE §5/TDD §8: shoal's `rm` trashes (journaled, undo-recoverable
/// via `apply`) rather than deleting outright, so `shoal_plan` must not
/// flatly call it "irreversible" — but an opaque external `sh { rm -rf }`
/// (a structurally different effect, `Effect::Opaque`, never
/// `Effect::FsDelete`) must never be reported reversible just because its
/// source text also says "rm -rf".
#[test]
fn mcp_shoal_plan_distinguishes_trash_rm_from_opaque_rm() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();
    let doomed = live._dir.path().join("doomed.txt");
    std::fs::write(&doomed, b"x").unwrap();

    let plan = call_tool(
        &mut facade,
        "shoal_plan",
        json!({"src": format!("rm {}", doomed.display())}),
    );
    assert_eq!(
        plan["structuredContent"]["reversibility"], "reversible",
        "shoal's rm trashes (journaled undo); a plan for it must not read irreversible: {plan}"
    );

    let opaque_plan = call_tool(
        &mut facade,
        "shoal_plan",
        json!({"src": format!("sh {{ rm -rf {} }}", doomed.display())}),
    );
    assert_eq!(
        opaque_plan["structuredContent"]["reversibility"], "irreversible",
        "an opaque external rm -rf must never be reported reversible: {opaque_plan}"
    );
}

// ---------------------------------------------------------------------------
// Raw JSON-RPC over the same socket (no facade): events round-trip.
// ---------------------------------------------------------------------------

fn raw_call(
    writer: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    id: i64,
    method: &str,
    params: Value,
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
    loop {
        let frame = read_frame_as_response(reader);
        if frame.id.as_i64() == Some(id) {
            return frame;
        }
    }
}

fn read_frame_as_response(reader: &mut BufReader<UnixStream>) -> Response {
    use std::io::BufRead;
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// `events.publish` → `events.read` round-trips on a `user.*` channel over the
/// raw wire (AGENT-SURFACE §4/§7 — the pair-shelling primitive).
#[test]
fn raw_events_publish_read_roundtrip_on_user_channel() {
    let live = LiveKernel::start();
    let mut stream = UnixStream::connect(&live.socket).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    raw_call(
        &mut stream,
        &mut reader,
        1,
        "session.attach",
        json!({"client":{"kind":"test","tty":false}}),
    );
    let published = raw_call(
        &mut stream,
        &mut reader,
        2,
        "events.publish",
        json!({"channel":"user.review","payload":{"$":"str","v":"lgtm"}}),
    );
    assert!(published.error.is_none());
    let read = raw_call(
        &mut stream,
        &mut reader,
        3,
        "events.read",
        json!({"channel":"user.review"}),
    );
    let events = read.result.unwrap()["events"].clone();
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["payload"], json!({"$":"str","v":"lgtm"}));
    assert_eq!(events[0]["channel"], "user.review");

    // Publishing to a kernel-owned channel is refused (only user.* is writable).
    let denied = raw_call(
        &mut stream,
        &mut reader,
        4,
        "events.publish",
        json!({"channel":"journal","payload":{"$":"int","v":1}}),
    );
    assert!(denied.error.is_some());
}

/// The channel↔wire bridge (AGENT-SURFACE §4 "one substrate"): an in-language
/// `channel("user.x").emit(...)` must reach wire subscribers/readers, and a
/// wire `events.publish` must be visible to in-language `latest()` — the two
/// event worlds used to be fully disjoint (the field test's last blocker).
#[test]
fn language_channel_emit_bridges_to_wire_bus_and_back() {
    let live = LiveKernel::start();
    // Connection 1: requests/responses only (no subscription, so no pushed
    // notification can ever interleave with a response frame here).
    let mut stream = UnixStream::connect(&live.socket).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    raw_call(
        &mut stream,
        &mut reader,
        1,
        "session.attach",
        json!({"client":{"kind":"test","tty":false}}),
    );
    // Connection 2: a dedicated subscriber that only ever receives pushes.
    let mut sub_stream = UnixStream::connect(&live.socket).unwrap();
    let mut sub_reader = BufReader::new(sub_stream.try_clone().unwrap());
    raw_call(
        &mut sub_stream,
        &mut sub_reader,
        1,
        "session.attach",
        json!({"client":{"kind":"test","tty":false}}),
    );
    let sub = raw_call(
        &mut sub_stream,
        &mut sub_reader,
        2,
        "events.subscribe",
        json!({"channel":"user.bridge"}),
    );
    assert!(sub.error.is_none());

    // language → wire: an evaluated emit lands on the kernel bus…
    let exec = raw_call(
        &mut stream,
        &mut reader,
        2,
        "exec",
        json!({"src":"channel(\"user.bridge\").emit(\"lang-ping\")","position":"stmt"}),
    );
    assert!(exec.error.is_none(), "emit exec failed: {:?}", exec.error);
    let read = raw_call(
        &mut stream,
        &mut reader,
        3,
        "events.read",
        json!({"channel":"user.bridge"}),
    );
    let events = read.result.unwrap()["events"].clone();
    let events = events.as_array().unwrap().clone();
    assert_eq!(events.len(), 1, "language emit must reach the wire bus");
    assert_eq!(events[0]["payload"], json!({"$":"str","v":"lang-ping"}));

    // …and is PUSHED to the live subscriber as an `event` notification.
    let frame = read_frame_raw(&mut sub_reader);
    assert_eq!(frame["method"], "event", "expected a push, got {frame}");
    assert_eq!(frame["params"]["channel"], "user.bridge");
    assert_eq!(
        frame["params"]["payload"],
        json!({"$":"str","v":"lang-ping"})
    );

    // wire → language: a wire publish is visible to in-language `latest()`.
    let published = raw_call(
        &mut stream,
        &mut reader,
        4,
        "events.publish",
        json!({"channel":"user.bridge","payload":"wire-pong"}),
    );
    assert!(published.error.is_none());
    let latest = raw_call(
        &mut stream,
        &mut reader,
        5,
        "exec",
        json!({"src":"channel(\"user.bridge\").latest()","position":"value"}),
    );
    let value = latest.result.unwrap()["value"].clone();
    assert_eq!(
        value,
        json!({"$":"str","v":"wire-pong"}),
        "wire publish must be visible to language latest()"
    );
}

/// Reads one raw frame (response OR notification) as loose JSON.
fn read_frame_raw(reader: &mut BufReader<UnixStream>) -> Value {
    use std::io::BufRead;
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}
