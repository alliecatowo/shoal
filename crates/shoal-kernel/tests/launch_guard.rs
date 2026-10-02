//! Out-of-process proof that parent loss before lifecycle commit cannot leave
//! a listener or valid supervisor authority behind.

use shoal_auth::TokenStore;
use shoal_proto::{AttachParams, ClientInfo, JSONRPC, Request, Response, write_frame};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn pre_release_parent_loss_revokes_authority_and_never_publishes_socket() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let store_path = state.join("tokens.json");
    let socket = root.path().join("guarded.sock");
    let (token, meta) = TokenStore::open(&store_path)
        .unwrap()
        .create(
            "test:managed-parent".into(),
            "supervisor".into(),
            Vec::new(),
            None,
        )
        .unwrap();
    let (read, write) = pipe().unwrap();
    let read_fd = read.as_raw_fd();
    let write_fd = write.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_shoal-kernel"));
    command
        .arg("--socket")
        .arg(&socket)
        .arg("--state-dir")
        .arg(&state)
        .arg("--token-store")
        .arg(&store_path)
        .arg("--launch-guard-fd")
        .arg(read_fd.to_string())
        .arg("--launch-guard-token-id")
        .arg(&meta.id)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // Model the supervisor's inherited-FD setup. Its process death closes
    // `write`; the child has no writer of its own and therefore observes EOF.
    // SAFETY: only async-signal-safe descriptor operations run before exec.
    unsafe {
        command.pre_exec(move || {
            if libc::close(write_fd) == -1 || libc::fcntl(read_fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(read);
    drop(write); // deterministic parent loss before the release byte

    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "guarded child did not self-terminate"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success(), "unreleased launch must fail closed");
    assert!(
        !socket.exists(),
        "unreleased child published a public socket"
    );
    assert!(
        TokenStore::open(&store_path)
            .unwrap()
            .validate_checked(&token)
            .unwrap()
            .is_none(),
        "unreleased supervisor bearer remained valid"
    );
}

#[test]
fn released_managed_kernel_accepts_its_supervisor_and_stops_cleanly() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let store_path = state.join("tokens.json");
    let socket = root.path().join("guarded.sock");
    let (token, meta) = TokenStore::open(&store_path)
        .unwrap()
        .create(
            "test:managed-parent".into(),
            "supervisor".into(),
            Vec::new(),
            None,
        )
        .unwrap();
    let (read, write) = pipe().unwrap();
    let mut write = std::fs::File::from(write);
    let read_fd = read.as_raw_fd();
    let write_fd = write.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_shoal-kernel"));
    command
        .arg("--socket")
        .arg(&socket)
        .arg("--state-dir")
        .arg(&state)
        .arg("--token-store")
        .arg(&store_path)
        .arg("--launch-guard-fd")
        .arg(read_fd.to_string())
        .arg("--launch-guard-token-id")
        .arg(&meta.id)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe descriptor operations run before exec.
    unsafe {
        command.pre_exec(move || {
            if libc::close(write_fd) == -1 || libc::fcntl(read_fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(read);
    write.write_all(&[1]).unwrap();
    drop(write);

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut client = loop {
        match UnixStream::connect(&socket) {
            Ok(stream) => break stream,
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "released kernel did not publish socket"
                );
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "released kernel exited early"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    let mut reader = BufReader::new(client.try_clone().unwrap());
    rpc(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        serde_json::to_value(AttachParams {
            session: None,
            token: Some(token),
            client: ClientInfo {
                kind: "managed-launch-test".into(),
                tty: false,
            },
        })
        .unwrap(),
    );
    rpc(
        &mut client,
        &mut reader,
        2,
        "kernel.shutdown",
        serde_json::json!({}),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "managed kernel did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn rpc(
    stream: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
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
    let response: Response = serde_json::from_str(&line).unwrap();
    assert!(response.error.is_none(), "{response:?}");
    response.result.unwrap_or(serde_json::Value::Null)
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: fds is writable storage for two descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe returned two distinct fresh descriptors.
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    for fd in [read.as_raw_fd(), write.as_raw_fd()] {
        // SAFETY: both descriptors are owned and live for this call.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}
