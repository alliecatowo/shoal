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
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(socket.exists(), "kernel must bind its socket");
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
