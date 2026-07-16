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

/// `resources/read` that is expected to fail — returns the whole response so
/// the test can assert an `error` object (unknown plan/task refs, §1).
fn read_resource_expecting_error(facade: &mut Facade, uri: &str) -> Value {
    let request = json!({
        "jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":uri}
    });
    facade
        .handle(&request)
        .expect("resources/read has a response")
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

/// AGENT-SURFACE §1 (`shoal://session/env`): the session environment view is
/// served from the session evaluator. A default-permissive human is granted a
/// value read (`env_read=["*"]`), so `granted:true` and the values travel
/// alongside the names.
#[test]
fn mcp_session_env_resource_is_served_with_values_for_the_default_human() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();

    let env = read_resource(&mut facade, "shoal://session/env");
    let sc = &env["structuredContent"];
    assert_eq!(
        sc["granted"], true,
        "the default-permissive human is granted an env value read: {sc}"
    );
    let names = sc["names"].as_array().expect("env names is a list");
    assert!(!names.is_empty(), "the session env is not empty: {sc}");
    let values = sc["env"].as_object().expect("granted env carries values");
    assert_eq!(
        values.len(),
        names.len(),
        "every named var carries a value when granted"
    );
}

/// AGENT-SURFACE §1 (`shoal://session/reef`): the reef resolution view is
/// served and correctly shaped — an `active_scope` (string or honest null when
/// no manifest is in scope) plus a `bindings` array. Host-independent: it only
/// asserts the shape, never a particular manifest state.
#[test]
fn mcp_session_reef_resource_is_served_and_shaped() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();

    let reef = read_resource(&mut facade, "shoal://session/reef");
    let sc = &reef["structuredContent"];
    assert!(
        sc.get("active_scope").is_some(),
        "reef view carries an active_scope field (may be null): {sc}"
    );
    assert!(
        sc["bindings"].is_array(),
        "reef view carries a bindings array: {sc}"
    );
}

/// AGENT-SURFACE §1 (`shoal://plan/{ref}`): a plan derived by `shoal_plan` is
/// afterward readable as a resource by its `plan:<hex>` ref — its effects,
/// reversibility, and verdict mirror what the `shoal_plan` call returned. An
/// unknown/expired ref is a clear not-found error, never a silent empty plan.
#[test]
fn mcp_plan_resource_reads_back_a_stored_plan_and_errors_on_unknown() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();
    let doomed = live._dir.path().join("doomed.txt");
    std::fs::write(&doomed, b"x").unwrap();

    let plan = call_tool(
        &mut facade,
        "shoal_plan",
        json!({"src": format!("rm {}", doomed.display())}),
    );
    let plan_ref = plan["structuredContent"]["plan_ref"]
        .as_str()
        .expect("shoal_plan returns a plan_ref")
        .to_string();
    // `plan:<hex>` short ref → `shoal://plan/<hex>` URI.
    let bare = plan_ref
        .strip_prefix("plan:")
        .expect("plan ref is plan:<hex>");
    let uri = format!("shoal://plan/{bare}");

    let read = read_resource(&mut facade, &uri);
    let sc = &read["structuredContent"];
    assert_eq!(
        sc["plan_ref"], plan_ref,
        "the plan resource round-trips its own ref: {sc}"
    );
    assert_eq!(
        sc["reversibility"], plan["structuredContent"]["reversibility"],
        "the resource's reversibility mirrors the plan call's: {sc}"
    );
    assert!(
        sc["effects"].is_array() && !sc["effects"].as_array().unwrap().is_empty(),
        "the plan resource carries its concrete effects: {sc}"
    );
    assert!(
        sc.get("verdict").is_some(),
        "the plan resource has a verdict"
    );
    assert!(sc.get("ast").is_some(), "the plan resource carries its ast");

    // An unknown plan ref is a not-found error, not an empty success.
    let missing = read_resource_expecting_error(&mut facade, "shoal://plan/deadbeefdeadbeef");
    assert!(
        missing.get("error").is_some(),
        "an unknown plan ref must error: {missing}"
    );
}

/// AGENT-SURFACE §1/§6 (`shoal://task/{id}/out`): the read side of a task's
/// output. A completed background task's captured output is reachable by URI —
/// the `/out` segment resolves the task's result value (previously ignored, so
/// the record was returned instead of the output). An unknown task errors.
#[test]
fn mcp_task_out_resource_reads_captured_output_and_errors_on_unknown() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();

    let bg = call_tool(
        &mut facade,
        "shoal_exec",
        json!({"src":"sh { echo hello-from-task }","background":true}),
    );
    let task_ref = bg["structuredContent"]["task"]
        .as_str()
        .expect("background exec returns a task ref")
        .to_string();
    let bare_id = task_ref.strip_prefix("task:").expect("task ref is task:N");

    // Wait for the task to finish and capture its output value.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let jobs = read_resource(&mut facade, "shoal://jobs");
        let tasks = jobs["structuredContent"].as_array().unwrap().clone();
        if let Some(task) = tasks.iter().find(|t| t["task"] == json!(task_ref))
            && task["state"] != "running"
            && task["state"] != "cancelling"
        {
            break;
        }
        assert!(Instant::now() < deadline, "task never finished");
        std::thread::sleep(Duration::from_millis(20));
    }

    let out = read_resource(&mut facade, &format!("shoal://task/{bare_id}/out"));
    let sc = &out["structuredContent"];
    assert!(
        !sc.is_null(),
        "a completed task's /out resolves to its captured output, not null: {sc}"
    );
    // The captured output is the `echo` outcome; its text carries the marker.
    assert!(
        serde_json::to_string(sc)
            .unwrap()
            .contains("hello-from-task"),
        "task /out carries the command's captured output: {sc}"
    );

    // An unknown task id is a not-found error.
    let missing = read_resource_expecting_error(&mut facade, "shoal://task/999999/out");
    assert!(
        missing.get("error").is_some(),
        "an unknown task's /out must error: {missing}"
    );
}

/// AGENT-SURFACE §1 (`shoal://val/blake3:{hex}`): the spec's short-ref value
/// URI form (with the `blake3:` algorithm prefix) must resolve the same CAS
/// blob as the bare-hex form — the prefix is stripped before the lookup.
#[test]
fn mcp_val_resource_accepts_the_blake3_prefixed_spec_form() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();

    // Run something so the journal CAS holds at least one output blob.
    call_tool(&mut facade, "shoal_exec", json!({"src":"1 + 2"}));
    let journal = read_resource(&mut facade, "shoal://journal");
    let entries = journal["structuredContent"].as_array().unwrap();
    let hash = entries
        .iter()
        .flat_map(|e| e["outputs"].as_array().cloned().unwrap_or_default())
        .find_map(|o| o["hash"].as_str().map(String::from))
        .expect("a journal entry carries at least one content-addressed output");

    let bare = read_resource(&mut facade, &format!("shoal://val/{hash}"));
    let prefixed = read_resource(&mut facade, &format!("shoal://val/blake3:{hash}"));
    assert_eq!(
        bare["structuredContent"], prefixed["structuredContent"],
        "val:blake3:<hex> resolves the same blob as the bare hex form"
    );
}

/// AGENT-SURFACE §8: `resources/list` advertises the newly-served static roots
/// (`session/env`, `session/reef`) and any open plan the session derived.
#[test]
fn mcp_resources_list_advertises_new_roots_and_open_plans() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();
    let doomed = live._dir.path().join("doomed.txt");
    std::fs::write(&doomed, b"x").unwrap();
    let plan = call_tool(
        &mut facade,
        "shoal_plan",
        json!({"src": format!("rm {}", doomed.display())}),
    );
    let plan_ref = plan["structuredContent"]["plan_ref"].as_str().unwrap();
    let bare = plan_ref.strip_prefix("plan:").unwrap();

    let list = facade
        .handle(&json!({"jsonrpc":"2.0","id":3,"method":"resources/list"}))
        .unwrap();
    let uris: Vec<String> = list["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap().to_string())
        .collect();
    assert!(
        uris.iter().any(|u| u == "shoal://session/env"),
        "session/env is advertised: {uris:?}"
    );
    assert!(
        uris.iter().any(|u| u == "shoal://session/reef"),
        "session/reef is advertised: {uris:?}"
    );
    assert!(
        uris.iter().any(|u| *u == format!("shoal://plan/{bare}")),
        "the open plan is advertised: {uris:?}"
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

// ---------------------------------------------------------------------------
// AGENT-SURFACE §10: an agent drives an interactive PTY program over the wire
// and reads back a rendered screen (never a byte wall).
// ---------------------------------------------------------------------------

/// `true` once `pid` is no longer a live process — `kill(pid, 0)` → `ESRCH`.
/// The OS-level no-leak proof: after `pty.close` the child must be reaped.
#[allow(clippy::cast_possible_wrap)] // pids fit in i32 in practice
fn process_is_gone(pid: u32) -> bool {
    let pid = pid as libc::pid_t;
    // SAFETY: signal 0 is the POSIX existence probe; it delivers nothing.
    unsafe {
        libc::kill(pid, 0) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
}

/// The full vertical slice over a REAL kernel + socket through the MCP facade:
/// open an interactive `cat` on a PTY, type a line + a named `Enter`, poll the
/// RENDERED screen until the echoed text appears at a moved cursor, then close
/// and prove the child was reaped (no leaked process). `cat` is deterministic
/// and present on every host: the tty line discipline echoes typed characters
/// and `cat` writes each completed line back, so the emulator's screen grid
/// carries the text — exercising open → send(named key) → read(rendered
/// screen) → close → reap end-to-end.
#[test]
fn mcp_pty_drive_cat_reads_rendered_screen_then_closes_and_reaps() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();

    // Open an interactive program on a 40x10 terminal.
    let opened = call_tool(
        &mut facade,
        "shoal_pty_open",
        json!({"cmd":"cat","cols":40,"rows":10}),
    );
    assert_ne!(opened["isError"], true, "pty.open must succeed: {opened}");
    let sc = &opened["structuredContent"];
    let pty_id = sc["pty_id"].as_str().expect("pty.open returns a pty_id");
    assert!(pty_id.starts_with("pty:"), "pty_id shape: {pty_id}");
    assert_eq!(sc["cols"], 40, "the requested size took: {sc}");
    assert_eq!(sc["rows"], 10);
    let pid = sc["pid"].as_u64().expect("pty.open returns the child pid") as u32;
    let pty_id = pty_id.to_string();

    // Type text then a NAMED Enter key — the key-name protocol, mixed in one
    // send as an array.
    let sent = call_tool(
        &mut facade,
        "shoal_pty_send",
        json!({"pty_id": pty_id, "input": ["hello-pty", {"key":"Enter"}]}),
    );
    assert_ne!(sent["isError"], true, "pty.send must succeed: {sent}");

    // Poll the RENDERED screen until the echoed line shows up. The screen is a
    // bounded array of text rows — no escape bytes.
    let deadline = Instant::now() + Duration::from_secs(5);
    let read = loop {
        let read = call_tool(&mut facade, "shoal_pty_read", json!({"pty_id": pty_id}));
        assert_ne!(read["isError"], true, "pty.read must succeed: {read}");
        let rsc = &read["structuredContent"];
        let screen = rsc["screen"]
            .as_array()
            .expect("screen is an array of rows");
        assert_eq!(screen.len(), 10, "screen has exactly `rows` rows: {rsc}");
        if screen
            .iter()
            .any(|row| row.as_str().is_some_and(|r| r.contains("hello-pty")))
        {
            break read;
        }
        assert!(
            Instant::now() < deadline,
            "rendered screen never showed the echoed line: {rsc}"
        );
        std::thread::sleep(Duration::from_millis(30));
    };
    let rsc = &read["structuredContent"];
    // The cursor advanced off row 0 — the emulator tracked the newline, proving
    // it is a real terminal emulator, not a raw byte passthrough.
    assert!(
        rsc["cursor"]["row"].as_u64().unwrap() >= 1,
        "cursor should have moved down after the newline: {rsc}"
    );
    assert_eq!(rsc["alive"], true, "cat is still running");
    assert_eq!(rsc["exit"], Value::Null, "an alive pty has a null exit");

    // Close terminates + reaps; the child must actually be gone (no leak).
    let closed = call_tool(&mut facade, "shoal_pty_close", json!({"pty_id": pty_id}));
    assert_eq!(
        closed["structuredContent"]["closed"], true,
        "closed: {closed}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !process_is_gone(pid) {
        assert!(
            Instant::now() < deadline,
            "the pty child must be reaped after pty.close — leaked pid {pid}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // A read on the now-closed pty is a clean not-found, not a hang/crash.
    let after = call_tool(&mut facade, "shoal_pty_read", json!({"pty_id": pty_id}));
    assert_eq!(
        after["isError"], true,
        "reading a closed pty is an error: {after}"
    );
}

/// AGENT-SURFACE §10 (`pty.list` / `shoal://pty` + `shoal://pty/{id}`): open
/// PTY sessions are first-class on the agent surface — discoverable via the
/// `shoal_pty_list` tool and the `shoal://pty` resource, and drill-in-able via
/// `shoal://pty/{id}` (the rendered screen), mirroring how an exec'd value
/// becomes a `shoal://` noun. Over a REAL kernel + socket through the MCP
/// facade: with no pty open the list is empty; open `cat`; assert `pty.list`
/// (both the tool and the `shoal://pty` resource) shows exactly that session
/// with the documented shape; read `shoal://pty/{id}` and assert the rendered
/// screen shape; close it; assert it leaves `pty.list` and its screen resource
/// then errors cleanly.
#[test]
fn mcp_pty_list_and_resources_track_open_sessions() {
    let live = LiveKernel::start();
    let mut facade = Facade::connect(&live.config()).unwrap();

    // Nothing open yet: the list tool AND the shoal://pty resource are empty.
    let empty = call_tool(&mut facade, "shoal_pty_list", json!({}));
    assert_ne!(empty["isError"], true, "pty.list must succeed: {empty}");
    assert!(
        empty["structuredContent"]["ptys"]
            .as_array()
            .unwrap()
            .is_empty(),
        "no ptys open yet: {empty}"
    );
    let empty_res = read_resource(&mut facade, "shoal://pty");
    assert!(
        empty_res["structuredContent"]["ptys"]
            .as_array()
            .unwrap()
            .is_empty(),
        "shoal://pty is empty with no ptys open: {empty_res}"
    );

    // Open an interactive cat on a 40x10 terminal.
    let opened = call_tool(
        &mut facade,
        "shoal_pty_open",
        json!({"cmd":"cat","cols":40,"rows":10}),
    );
    assert_ne!(opened["isError"], true, "pty.open must succeed: {opened}");
    let pty_id = opened["structuredContent"]["pty_id"]
        .as_str()
        .expect("pty.open returns a pty_id")
        .to_string();
    let bare = pty_id
        .strip_prefix("pty:")
        .expect("pty id is pty:N")
        .to_string();

    // shoal_pty_list shows exactly that session with the documented shape.
    let listed = call_tool(&mut facade, "shoal_pty_list", json!({}));
    let ptys = listed["structuredContent"]["ptys"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(ptys.len(), 1, "exactly the one open pty: {listed}");
    let entry = &ptys[0];
    assert_eq!(entry["pty_id"], json!(pty_id));
    assert_eq!(entry["cmd"], "cat");
    assert_eq!(entry["cols"], 40);
    assert_eq!(entry["rows"], 10);
    assert_eq!(entry["alive"], true);
    assert!(entry["pid"].as_u64().unwrap() > 0);

    // The shoal://pty resource carries the same list.
    let res = read_resource(&mut facade, "shoal://pty");
    let res_ptys = res["structuredContent"]["ptys"].as_array().unwrap();
    assert_eq!(res_ptys.len(), 1, "shoal://pty mirrors pty.list: {res}");
    assert_eq!(res_ptys[0]["pty_id"], json!(pty_id));

    // resources/list advertises the shoal://pty root AND the open session entry.
    let list = facade
        .handle(&json!({"jsonrpc":"2.0","id":3,"method":"resources/list"}))
        .unwrap();
    let uris: Vec<String> = list["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap().to_string())
        .collect();
    assert!(
        uris.iter().any(|u| u == "shoal://pty"),
        "shoal://pty root advertised: {uris:?}"
    );
    assert!(
        uris.iter().any(|u| *u == format!("shoal://pty/{bare}")),
        "the open pty is advertised: {uris:?}"
    );

    // shoal://pty/{id} drills into the one session's RENDERED screen — the same
    // shape pty.read returns (bounded rows array, cursor, alive), no byte wall.
    let screen = read_resource(&mut facade, &format!("shoal://pty/{bare}"));
    let ssc = &screen["structuredContent"];
    assert_eq!(ssc["pty_id"], json!(pty_id));
    assert_eq!(ssc["cols"], 40);
    assert_eq!(ssc["rows"], 10);
    assert_eq!(ssc["alive"], true);
    let rows = ssc["screen"]
        .as_array()
        .expect("screen is an array of rows");
    assert_eq!(rows.len(), 10, "screen has exactly `rows` rows: {ssc}");
    assert!(ssc["cursor"].is_object(), "screen carries a cursor: {ssc}");

    // Close terminates + reaps; the pty leaves pty.list and its screen resource
    // then errors cleanly (a closed id is an opaque not-found, never a hang).
    let closed = call_tool(&mut facade, "shoal_pty_close", json!({"pty_id": pty_id}));
    assert_eq!(
        closed["structuredContent"]["closed"], true,
        "closed: {closed}"
    );
    let after = call_tool(&mut facade, "shoal_pty_list", json!({}));
    assert!(
        after["structuredContent"]["ptys"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a closed pty must leave pty.list: {after}"
    );
    let missing = read_resource_expecting_error(&mut facade, &format!("shoal://pty/{bare}"));
    assert!(
        missing.get("error").is_some(),
        "reading a closed pty's screen must error: {missing}"
    );
}
