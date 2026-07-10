use shoal_proto::{AttachParams, ClientInfo, ExecParams, JSONRPC, Request, Response, write_frame};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Route a spawned daemon's stderr to its own file inside its tempdir
/// (rather than piping it and never draining it, or inheriting the test
/// binary's stderr where two daemons running concurrently can interleave
/// their output into a single unattributable, garbled line). Callers can
/// read this file back to attribute a panic/error message to a specific
/// daemon instance.
fn daemon_stderr_file(dir: &Path) -> (std::fs::File, std::path::PathBuf) {
    let path = dir.join("daemon-stderr.log");
    let file = std::fs::File::create(&path).unwrap();
    (file, path)
}

/// Read back a daemon's captured stderr for inclusion in a failure message.
fn read_daemon_stderr(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| format!("<could not read {path:?}: {e}>"))
}

/// Only one live `shoal-kernel` daemon per test binary at a time.
///
/// Both tests below spawn a real daemon child process, talk to it over a
/// real Unix socket, and signal it directly by PID (`kill(child.id(), ...)`)
/// to shut it down. The default Rust test harness runs `#[test]` fns
/// concurrently on separate threads, so with no serialization both daemons
/// are alive at once. On macOS CI this has produced a daemon (this file's
/// second test) exiting cleanly and silently — no panic, no logged error,
/// just its `serve_until` accept loop ending, which only happens once the
/// `ctrlc` handler observes a termination signal — immediately after the
/// *other* test's own daemon lifecycle (which also sends a termination
/// signal, to what it believes is its own child) ran concurrently. Whatever
/// the exact OS-level mechanism, forcing the two daemons to never coexist
/// removes any possibility of one test's signal/process handling touching
/// the other's daemon, at negligible cost (each test finishes in well under
/// a second).
static ONLY_ONE_DAEMON_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn daemon_binds_secure_socket_and_attaches() {
    let _serialize = ONLY_ONE_DAEMON_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("run/session.sock");
    let (stderr_file, stderr_path) = daemon_stderr_file(temp.path());
    let mut child = Command::new(env!("CARGO_BIN_EXE_shoal-kernel"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--state-dir",
            temp.path().join("state").to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(stderr_file)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        socket.exists(),
        "daemon stderr:\n{}",
        read_daemon_stderr(&stderr_path)
    );
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut stream = UnixStream::connect(&socket).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    write_frame(
        &mut stream,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 1.into(),
            method: "session.attach".into(),
            params: serde_json::to_value(AttachParams {
                session: None,
                token: None,
                client: ClientInfo {
                    kind: "test".into(),
                    tty: false,
                },
            })
            .unwrap(),
        },
    )
    .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let response: Response = serde_json::from_str(&line).unwrap();
    assert!(response.error.is_none());
    unsafe {
        kill(child.id() as i32, 2);
    }
    assert!(
        child.wait().unwrap().success(),
        "daemon stderr:\n{}",
        read_daemon_stderr(&stderr_path)
    );
    assert!(!socket.exists());
}
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

fn call(
    stream: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    id: i64,
    method: &str,
    params: serde_json::Value,
) -> Response {
    write_frame(
        stream,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: id.into(),
            method: method.into(),
            params,
        },
    )
    .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Live end-to-end check of the elision rule (AGENT-SURFACE §3): a real
/// `shoal-kernel` process on a real Unix socket, a real 150-file directory,
/// a real `ls` exec over the wire. Confirms the *before* (what an unelided
/// 150-row table would look like) against the *after* (the elided ref that
/// actually travels).
#[test]
fn live_kernel_elides_a_big_table_over_the_wire() {
    let _serialize = ONLY_ONE_DAEMON_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("run/session.sock");
    let bigdir = temp.path().join("bigdir");
    std::fs::create_dir_all(&bigdir).unwrap();
    for i in 0..150 {
        std::fs::write(bigdir.join(format!("f{i:04}.txt")), b"x").unwrap();
    }
    let (stderr_file, stderr_path) = daemon_stderr_file(temp.path());
    let mut child = Command::new(env!("CARGO_BIN_EXE_shoal-kernel"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--state-dir",
            temp.path().join("state").to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(stderr_file)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        socket.exists(),
        "live kernel must bind its socket; daemon stderr:\n{}",
        read_daemon_stderr(&stderr_path)
    );

    // The rest of the exchange runs inside `catch_unwind` purely so that,
    // on any failure (e.g. the daemon's connection closing unexpectedly),
    // we can attribute it by attaching the daemon's own captured stderr —
    // otherwise a mid-exchange failure (a closed socket, a panic in the
    // daemon) reports only an opaque `io::Error` / `Option::unwrap` panic
    // in the test with no indication of what the daemon-side cause was.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut stream = UnixStream::connect(&socket).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let attach = call(
            &mut stream,
            &mut reader,
            1,
            "session.attach",
            serde_json::to_value(AttachParams {
                session: None,
                token: None,
                client: ClientInfo {
                    kind: "test".into(),
                    tty: false,
                },
            })
            .unwrap(),
        );
        assert!(attach.error.is_none());

        // Diagnostic: is the daemon *process* itself still alive right
        // before we send the second request? A prior investigation found
        // the daemon's own stderr held nothing beyond its startup "ready"
        // line when this connection later failed with a broken pipe, which
        // rules out a panic/logged error on the daemon side but leaves two
        // very different possibilities open — the whole process already
        // exited (try_wait returns Some(status)) vs. only this connection
        // was closed while the daemon keeps running (try_wait returns
        // None). This is printed via the test's own stdout/stderr (which
        // the harness captures per-test), not the daemon's file, so it
        // survives regardless of which of the two turns out to be true.
        match child.try_wait() {
            Ok(None) => eprintln!(
                "[diag] daemon (pid {}) still running before exec call",
                child.id()
            ),
            Ok(Some(status)) => eprintln!(
                "[diag] daemon (pid {}) ALREADY EXITED before exec call: {status}",
                child.id()
            ),
            Err(e) => eprintln!("[diag] try_wait failed: {e}"),
        }

        let exec = call(
            &mut stream,
            &mut reader,
            2,
            "exec",
            serde_json::to_value(ExecParams {
                src: format!("ls {}", bigdir.display()),
                mode: "run".into(),
                position: "stmt".into(),
                asynchronous: false,
                elide: None,
            })
            .unwrap(),
        );
        let result = exec.result.expect("live `ls` over 150 files must succeed");
        let out = &result["value"]["out"];

        // BEFORE (what the wire would have carried without §3): a `Table`
        // whose `cols` map every column name to a 150-long array of tagged
        // cells — easily tens of KB for a directory listing with
        // name/size/modified. AFTER (what actually arrives): shape only.
        assert_eq!(
            out["$"], "ref",
            "150 rows over the wire, live, must arrive elided: {out}"
        );
        assert_eq!(out["of"], "table");
        assert_eq!(
            out["n"], 150,
            "the full count still travels even though the rows don't"
        );
        assert_eq!(
            out["cols"]["name"], "path",
            "column *schema* travels (name -> type)"
        );
        assert!(
            out["cols"].get("size").is_some(),
            "every table column's type is in the shape, not just the ones previewed"
        );
        assert_eq!(out["preview"]["$"], "table");
        assert_eq!(
            out["preview"]["n"], 5,
            "preview is capped at 5 rows, not 150"
        );
        let preview_names_len = out["preview"]["cols"]["name"].as_array().unwrap().len();
        assert_eq!(preview_names_len, 5);
        assert!(!out["render_head"].as_str().unwrap().is_empty());

        let elided_bytes = serde_json::to_vec(out).unwrap().len();
        assert!(
            elided_bytes < 4 * 1024,
            "the elided response itself must stay small (was {elided_bytes} bytes) — \
             a real un-elided 150-row `ls` table would run several times that"
        );
    }));

    if let Err(payload) = outcome {
        // The daemon may still be alive (e.g. the failure was a client-side
        // assertion) or already gone (e.g. its connection closed); either
        // way, best-effort reap it so it can't linger, then surface its
        // stderr alongside the original panic message.
        let _ = child.kill();
        let _ = child.wait();
        let msg = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "<non-string panic payload>".into());
        panic!(
            "live_kernel_elides_a_big_table_over_the_wire failed: {msg}\n\
             --- daemon stderr ({}) ---\n{}",
            stderr_path.display(),
            read_daemon_stderr(&stderr_path)
        );
    }

    unsafe {
        kill(child.id() as i32, 2);
    }
    assert!(
        child.wait().unwrap().success(),
        "daemon stderr:\n{}",
        read_daemon_stderr(&stderr_path)
    );
}
