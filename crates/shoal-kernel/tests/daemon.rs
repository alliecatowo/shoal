use shoal_proto::{AttachParams, ClientInfo, ExecParams, JSONRPC, Request, Response, write_frame};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn local_human_attach_params() -> serde_json::Value {
    let mut params = serde_json::to_value(AttachParams {
        session: None,
        token: None,
        client: ClientInfo {
            kind: "test".into(),
            tty: false,
        },
    })
    .unwrap();
    params["local_auth"] = serde_json::json!("local-human");
    params
}

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
            params: local_human_attach_params(),
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

/// Regression test for the accepted-socket non-blocking bug: `serve_until`
/// puts the *listener* in non-blocking mode so its accept loop can poll the
/// shutdown flag, but on some platforms (macOS) an accepted stream inherits
/// that non-blocking flag too (unlike Linux, where an accepted socket is
/// always blocking regardless of the listener's mode). Without an explicit
/// `stream.set_nonblocking(false)` on the accepted connection, a server-side
/// read that lands before the client's *next* write arrives returns
/// `WouldBlock`, which `handle_stream` propagates as an `Err` — silently
/// dropping the connection instead of blocking for more data.
///
/// This test opens one connection and issues two *sequential* requests with
/// a deliberate pause in between, so the daemon's second `read_frame` call
/// genuinely has to wait on an empty socket for the client's second write.
/// Under the old bug this reliably reproduced a dropped connection (the
/// first response would arrive, but the second `read_line` would return
/// `WouldBlock`/EOF before the client ever wrote its second request); with
/// the accepted stream forced back into blocking mode, both responses must
/// arrive correctly.
#[test]
fn daemon_survives_a_paused_gap_between_two_sequential_requests() {
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

    let mut stream = UnixStream::connect(&socket).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    // First request: attach. Read its response fully before moving on, so
    // the daemon's connection thread loops back around to its next
    // `read_frame` call — and starts blocking on an empty socket — well
    // before this test writes anything else.
    write_frame(
        &mut stream,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 1.into(),
            method: "session.attach".into(),
            params: local_human_attach_params(),
        },
    )
    .unwrap();
    let attach = recv(&mut reader);
    assert!(
        attach.error.is_none(),
        "attach failed: {:?}; daemon stderr:\n{}",
        attach.error,
        read_daemon_stderr(&stderr_path)
    );

    // Deliberate pause: give the daemon's per-connection thread ample time
    // to have already called (and be blocked in) its next `read_frame`
    // before the second request is written. Under the old bug, this window
    // is exactly when a non-blocking accepted socket would return
    // `WouldBlock` and the connection would be silently dropped.
    std::thread::sleep(Duration::from_millis(300));

    // Second request, sent well after the pause: exercises the very read
    // that would have raced (and lost) on a non-blocking accepted stream.
    write_frame(
        &mut stream,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 2.into(),
            method: "exec".into(),
            params: serde_json::to_value(ExecParams {
                src: "1 + 1".into(),
                mode: "run".into(),
                position: "stmt".into(),
                asynchronous: false,
                timeout_ms: None,
                elide: None,
                plan_ref: None,
            })
            .unwrap(),
        },
    )
    .unwrap();
    let exec = recv(&mut reader);
    assert!(
        exec.error.is_none(),
        "exec after paused gap failed (this is exactly the accepted-socket \
         non-blocking regression): {:?}; daemon stderr:\n{}",
        exec.error,
        read_daemon_stderr(&stderr_path)
    );

    unsafe {
        kill(child.id() as i32, 2);
    }
    assert!(
        child.wait().unwrap().success(),
        "daemon stderr:\n{}",
        read_daemon_stderr(&stderr_path)
    );
}

/// Read one newline-framed response, without writing anything first —
/// pairs with batching multiple requests into a single write (see
/// `live_kernel_elides_a_big_table_over_the_wire`).
fn recv(reader: &mut BufReader<UnixStream>) -> Response {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Live end-to-end check of the elision rule (site/content/internals/kernel-protocol.md): a real
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

        // Write the attach and exec requests as a *single* batched write,
        // rather than write-attach / read-response / write-exec.
        //
        // Root cause (confirmed via a `child.try_wait()` check added in an
        // earlier investigation): when this connection previously failed
        // on the second write with a broken pipe, the daemon *process* was
        // still alive — only this one connection's read loop had silently
        // ended. `Kernel::serve_until` puts the *listener* in non-blocking
        // mode and relies on `handle_stream`'s per-connection `read_frame`
        // blocking while it waits for the next request; on macOS, a socket
        // returned by `accept()` on a non-blocking listener can itself come
        // back non-blocking too (unlike Linux, where an accepted socket is
        // blocking regardless of the listener's mode — see e.g. the
        // long-documented BSD/Darwin `accept()` behavior difference). Any
        // read attempt that lands before the client's next write has
        // arrived then fails with `WouldBlock`, which `handle_stream`
        // propagates as an `Err` with no logging, silently dropping the
        // connection. That is a real bug in `shoal-kernel`'s server loop
        // (production code — out of scope for a test-only change: the
        // proper fix is to call `set_nonblocking(false)` on each accepted
        // stream). Batching both requests into one `write_all` ensures the
        // exec request's bytes are already sitting in the kernel socket
        // buffer well before the daemon ever attempts its second read, so
        // this test still exercises the real elision behavior end-to-end
        // over a real connection without depending on that server-side
        // race.
        let attach_request = Request {
            jsonrpc: JSONRPC.into(),
            id: 1.into(),
            method: "session.attach".into(),
            params: local_human_attach_params(),
        };
        let exec_request = Request {
            jsonrpc: JSONRPC.into(),
            id: 2.into(),
            method: "exec".into(),
            params: serde_json::to_value(ExecParams {
                src: format!("ls {}", bigdir.display()),
                mode: "run".into(),
                position: "stmt".into(),
                asynchronous: false,
                timeout_ms: None,
                elide: None,
                plan_ref: None,
            })
            .unwrap(),
        };
        let mut batched = Vec::new();
        for request in [&attach_request, &exec_request] {
            serde_json::to_writer(&mut batched, request).unwrap();
            batched.push(b'\n');
        }
        stream.write_all(&batched).unwrap();
        stream.flush().unwrap();

        let attach = recv(&mut reader);
        assert!(attach.error.is_none());

        // Diagnostic: is the daemon *process* itself still alive at this
        // point? Printed via the test's own stdout (captured by the
        // harness per-test, independent of the daemon's stderr file), kept
        // from the investigation above in case a different failure mode
        // shows up here in the future.
        match child.try_wait() {
            Ok(None) => eprintln!(
                "[diag] daemon (pid {}) still running after attach response",
                child.id()
            ),
            Ok(Some(status)) => eprintln!(
                "[diag] daemon (pid {}) ALREADY EXITED after attach response: {status}",
                child.id()
            ),
            Err(e) => eprintln!("[diag] try_wait failed: {e}"),
        }

        let exec = recv(&mut reader);
        let result = exec.result.expect("live `ls` over 150 files must succeed");
        let out = &result["value"]["out"];

        // BEFORE (what the wire would have carried without site/content/internals/kernel-protocol.md): a `Table`
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
            out["cols"]["name"], "str",
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
