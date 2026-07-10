use shoal_proto::{AttachParams, ClientInfo, JSONRPC, Request, Response, write_frame};
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
