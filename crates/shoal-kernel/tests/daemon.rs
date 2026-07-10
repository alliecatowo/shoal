use shoal_proto::{AttachParams, ClientInfo, ExecParams, JSONRPC, Request, Response, write_frame};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn daemon_binds_secure_socket_and_attaches() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("run/session.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_shoal-kernel"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--state-dir",
            temp.path().join("state").to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists());
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
    assert!(child.wait().unwrap().success());
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
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("run/session.sock");
    let bigdir = temp.path().join("bigdir");
    std::fs::create_dir_all(&bigdir).unwrap();
    for i in 0..150 {
        std::fs::write(bigdir.join(format!("f{i:04}.txt")), b"x").unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_shoal-kernel"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--state-dir",
            temp.path().join("state").to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "live kernel must bind its socket");

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

    // BEFORE (what the wire would have carried without §3): a `Table` whose
    // `cols` map every column name to a 150-long array of tagged cells —
    // easily tens of KB for a directory listing with name/size/modified.
    // AFTER (what actually arrives): shape only.
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

    unsafe {
        kill(child.id() as i32, 2);
    }
    assert!(child.wait().unwrap().success());
}
