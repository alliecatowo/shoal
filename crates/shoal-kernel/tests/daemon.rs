use shoal_journal::{Journal, JournalQuery};
use shoal_proto::error_code::AUTH_FAILED;
use shoal_proto::{AttachParams, ClientInfo, ExecParams, JSONRPC, Request, Response, write_frame};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn credentialed_admin_attach_params(token: &str) -> serde_json::Value {
    serde_json::to_value(AttachParams {
        session: None,
        token: Some(token.to_string()),
        client: ClientInfo {
            kind: "test".into(),
            tty: false,
        },
    })
    .unwrap()
}

fn create_admin_token(state: &Path) -> String {
    shoal_auth::TokenStore::open(state.join("tokens.json"))
        .unwrap()
        .create(
            format!("uid:{}", unsafe { geteuid() }),
            "supervisor".into(),
            vec![],
            None,
        )
        .unwrap()
        .0
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

/// Serialize tests that signal or intentionally coexist with real kernel
/// children. Individual tests can still spawn multiple kernels deliberately.
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
fn embedded_fd_is_private_trust_without_a_public_listener() {
    const SIGINT: i32 = 2;

    let _serialize = ONLY_ONE_DAEMON_AT_A_TIME
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    let config_root = temp.path().join("config");
    let config_dir = config_root.join("shoal");
    std::fs::create_dir_all(&config_dir).unwrap();
    let init = temp.path().join("interactive-init.shoal");
    std::fs::write(&init, "env.FROM_PRIVATE_INIT = 'yes'\n").unwrap();
    std::fs::write(
        config_dir.join("shoal.toml"),
        format!(
            "[init]\nfiles = [{}]\n",
            serde_json::to_string(init.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    let expected_socket = runtime.join("shoal/default.sock");
    let (mut child, mut private, stderr_path) =
        spawn_embedded_kernel(temp.path(), &state, &runtime, Some(&config_root), "single");
    let mut private_reader = BufReader::new(private.try_clone().unwrap());
    attach_embedded(&mut private, &mut private_reader, 1, "embedded");

    write_frame(
        &mut private,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 2.into(),
            method: "exec".into(),
            params: serde_json::json!({
                "src":"env.FROM_PRIVATE_INIT",
                "position":"value"
            }),
        },
    )
    .unwrap();
    assert_eq!(
        recv(&mut private_reader).result.unwrap()["value"],
        serde_json::json!({"$":"str","v":"yes"}),
        "the trusted private interactive profile must run configured init files"
    );

    assert!(
        !expected_socket.exists(),
        "private embedded mode must not create a listener"
    );
    assert_eq!(unsafe { kill(child.id() as i32, SIGINT) }, 0);
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        child.try_wait().unwrap().is_none(),
        "SIGINT killed embedded kernel:\n{}",
        read_daemon_stderr(&stderr_path)
    );
    write_frame(
        &mut private,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 3.into(),
            method: "parse".into(),
            params: serde_json::json!({"src":"1 + 2"}),
        },
    )
    .unwrap();
    assert!(recv(&mut private_reader).error.is_none());

    drop(private);
    drop(private_reader);
    wait_for_embedded_exit(&mut child, &stderr_path);
    assert!(!expected_socket.exists());
}

#[test]
fn two_private_embedded_kernels_can_share_state_without_contending_on_a_socket() {
    let _serialize = ONLY_ONE_DAEMON_AT_A_TIME
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    let expected_socket = runtime.join("shoal/default.sock");
    for round in 0..20 {
        // Spawn both before waiting for either readiness frame. This is real
        // concurrent state initialization, not two sequential starts whose
        // lifetimes merely overlap.
        let label_a = format!("stress-{round}-a");
        let label_b = format!("stress-{round}-b");
        let (mut child_a, mut private_a, stderr_a) =
            spawn_embedded_kernel_unready(temp.path(), &state, &runtime, None, &label_a);
        let (mut child_b, mut private_b, stderr_b) =
            spawn_embedded_kernel_unready(temp.path(), &state, &runtime, None, &label_b);
        await_embedded_ready(&mut child_a, &private_a, &stderr_a);
        await_embedded_ready(&mut child_b, &private_b, &stderr_b);
        let mut reader_a = BufReader::new(private_a.try_clone().unwrap());
        let mut reader_b = BufReader::new(private_b.try_clone().unwrap());

        attach_embedded(&mut private_a, &mut reader_a, 1, "human-a");
        attach_embedded(&mut private_b, &mut reader_b, 2, "human-b");
        assert_ne!(child_a.id(), child_b.id());
        assert!(child_a.try_wait().unwrap().is_none());
        assert!(child_b.try_wait().unwrap().is_none());
        assert!(
            !expected_socket.exists(),
            "isolated embedded kernels must not contend on a listener"
        );

        drop(private_a);
        drop(reader_a);
        drop(private_b);
        drop(reader_b);
        wait_for_embedded_exit(&mut child_a, &stderr_a);
        wait_for_embedded_exit(&mut child_b, &stderr_b);
    }
}

#[test]
fn private_embedded_kernel_coexists_with_a_durable_public_kernel() {
    let _serialize = ONLY_ONE_DAEMON_AT_A_TIME
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("run/public.sock");
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    let admin_token = create_admin_token(&state);
    let (public_stderr_file, public_stderr_path) = daemon_stderr_file(temp.path());
    let mut public_child = Command::new(env!("CARGO_BIN_EXE_shoal-kernel"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(public_stderr_file)
        .spawn()
        .unwrap();
    wait_for_socket(&socket, &public_stderr_path);

    let mut public = UnixStream::connect(&socket).unwrap();
    let mut public_reader = BufReader::new(public.try_clone().unwrap());
    write_frame(
        &mut public,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 2.into(),
            method: "session.attach".into(),
            params: credentialed_admin_attach_params(&admin_token),
        },
    )
    .unwrap();
    assert!(recv(&mut public_reader).error.is_none());
    write_frame(
        &mut public,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 3.into(),
            method: "kernel.status".into(),
            params: serde_json::json!({}),
        },
    )
    .unwrap();
    assert_eq!(
        recv(&mut public_reader).result.unwrap()["pid"],
        public_child.id()
    );
    for round in 0..10_u64 {
        let label = format!("coexist-{round}");
        let (mut embedded_child, mut private, embedded_stderr) =
            spawn_embedded_kernel(temp.path(), &state, &runtime, None, &label);
        let mut private_reader = BufReader::new(private.try_clone().unwrap());
        attach_embedded(&mut private, &mut private_reader, 10 + round, "human");
        assert_ne!(public_child.id(), embedded_child.id());

        drop(private);
        drop(private_reader);
        wait_for_embedded_exit(&mut embedded_child, &embedded_stderr);
        write_frame(
            &mut public,
            &Request {
                jsonrpc: JSONRPC.into(),
                id: (100 + round).into(),
                method: "kernel.status".into(),
                params: serde_json::json!({}),
            },
        )
        .unwrap();
        assert!(recv(&mut public_reader).error.is_none());
    }
    write_frame(
        &mut public,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 200.into(),
            method: "kernel.shutdown".into(),
            params: serde_json::json!({}),
        },
    )
    .unwrap();
    assert_eq!(recv(&mut public_reader).result.unwrap()["stopping"], true);
    assert!(
        public_child.wait().unwrap().success(),
        "public daemon stderr:\n{}",
        read_daemon_stderr(&public_stderr_path)
    );
}

#[test]
fn embedded_fd_rejects_a_descriptor_that_was_not_inherited() {
    let _serialize = ONLY_ONE_DAEMON_AT_A_TIME
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    let output = Command::new(env!("CARGO_BIN_EXE_shoal-kernel"))
        .args([
            "--embedded-fd",
            "999999",
            "--state-dir",
            state.to_str().unwrap(),
        ])
        .env("XDG_RUNTIME_DIR", &runtime)
        .env_remove("SHOAL_SOCKET")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not an inherited open descriptor"),
        "unexpected error: {stderr}"
    );
    assert!(!runtime.join("shoal/default.sock").exists());
}

fn spawn_embedded_kernel(
    dir: &Path,
    state: &Path,
    runtime: &Path,
    config_root: Option<&Path>,
    label: &str,
) -> (Child, UnixStream, PathBuf) {
    let (mut child, private, stderr_path) =
        spawn_embedded_kernel_unready(dir, state, runtime, config_root, label);
    await_embedded_ready(&mut child, &private, &stderr_path);
    (child, private, stderr_path)
}

fn spawn_embedded_kernel_unready(
    dir: &Path,
    state: &Path,
    runtime: &Path,
    config_root: Option<&Path>,
    label: &str,
) -> (Child, UnixStream, PathBuf) {
    const EMBEDDED_FD: i32 = 3;
    const F_GETFD: i32 = 1;
    const F_SETFD: i32 = 2;
    const FD_CLOEXEC: i32 = 1;

    let (private, child_end) = UnixStream::pair().unwrap();
    private
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let inherited_fd = child_end.as_raw_fd();
    let stderr_path = dir.join(format!("embedded-{label}-stderr.log"));
    let stderr_file = std::fs::File::create(&stderr_path).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_shoal-kernel"));
    command
        .args(["--embedded-fd", "3", "--state-dir", state.to_str().unwrap()])
        .env("XDG_RUNTIME_DIR", runtime)
        .env_remove("SHOAL_SOCKET")
        .stdout(Stdio::null())
        .stderr(stderr_file);
    if let Some(config_root) = config_root {
        command.env("XDG_CONFIG_HOME", config_root);
    }
    // SAFETY: only descriptor syscalls run between fork and exec. `dup2`
    // clears close-on-exec unless source and destination are already equal;
    // the explicit fcntl covers that edge case too.
    unsafe {
        command.pre_exec(move || {
            if dup2(inherited_fd, EMBEDDED_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = fcntl(EMBEDDED_FD, F_GETFD, 0);
            if flags == -1 || fcntl(EMBEDDED_FD, F_SETFD, flags & !FD_CLOEXEC) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    drop(child_end);
    (child, private, stderr_path)
}

fn await_embedded_ready(child: &mut Child, private: &UnixStream, stderr_path: &Path) {
    let mut ready_reader = BufReader::new(private.try_clone().unwrap());
    let mut ready_line = String::new();
    let ready_result = ready_reader.read_line(&mut ready_line);
    assert!(
        ready_result.is_ok_and(|bytes| bytes > 0),
        "embedded child failed before readiness (status {:?}):\n{}",
        child.try_wait(),
        read_daemon_stderr(stderr_path)
    );
    let ready: serde_json::Value = serde_json::from_str(ready_line.trim_end()).unwrap();
    assert_eq!(ready["shoal_embedded"]["ready"], true);
    assert_eq!(ready["shoal_embedded"]["protocol"], 1);
}

fn attach_embedded(
    private: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    id: u64,
    session: &str,
) {
    write_frame(
        private,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: id.into(),
            method: "session.attach".into(),
            params: serde_json::json!({
                "local_auth":"local-human",
                "session":session,
                "client":{"kind":"shoal-repl","tty":true}
            }),
        },
    )
    .unwrap();
    let trusted = recv(reader).result.unwrap();
    assert_eq!(trusted["connection_trust"], "embedded-human");
}

fn wait_for_socket(socket: &Path, stderr_path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        socket.exists(),
        "daemon did not bind listener:\n{}",
        read_daemon_stderr(stderr_path)
    );
}

fn wait_for_embedded_exit(child: &mut Child, stderr_path: &Path) {
    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "embedded kernel did not exit after private endpoint closed:\n{}",
            read_daemon_stderr(stderr_path)
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "embedded kernel exit: {status}; stderr:\n{}",
        read_daemon_stderr(stderr_path)
    );
}

#[test]
fn daemon_binds_secure_socket_and_attaches() {
    let _serialize = ONLY_ONE_DAEMON_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("run/session.sock");
    let state = temp.path().join("state");
    let admin_token = create_admin_token(&state);
    let config_root = temp.path().join("config");
    let config_dir = config_root.join("shoal");
    std::fs::create_dir_all(&config_dir).unwrap();
    let init = temp.path().join("init.shoal");
    // `init.files` are an interactive-shell surface. An agent Session must not
    // evaluate even a configured, malformed init file implicitly.
    std::fs::write(&init, "\"unterminated\n").unwrap();
    std::fs::write(
        config_dir.join("shoal.toml"),
        format!(
            "[env]\nFROM_CONFIG = \"config-value\"\n[init]\nfiles = [{}]\n[journal]\nenabled = false\n",
            serde_json::to_string(init.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    let (stderr_file, stderr_path) = daemon_stderr_file(temp.path());
    let mut child = Command::new(env!("CARGO_BIN_EXE_shoal-kernel"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ])
        .env("XDG_CONFIG_HOME", &config_root)
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
    // This test process has the exact same effective UID as its daemon child.
    // Owning the socket file and truthfully claiming a TTY therefore still
    // must not let an arbitrary sibling process manufacture human authority.
    write_frame(
        &mut stream,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 0.into(),
            method: "session.attach".into(),
            params: serde_json::json!({
                "local_auth": "local-human",
                "client": {"kind": "same-uid-adversary", "tty": true}
            }),
        },
    )
    .unwrap();
    let raw_denial = recv(&mut reader).error.unwrap();
    assert_eq!(raw_denial.code, AUTH_FAILED);
    assert_eq!(raw_denial.data.unwrap()["human_presence_supported"], false);

    write_frame(
        &mut stream,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 1.into(),
            method: "session.attach".into(),
            params: credentialed_admin_attach_params(&admin_token),
        },
    )
    .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let response: Response = serde_json::from_str(&line).unwrap();
    assert!(response.error.is_none());
    write_frame(
        &mut stream,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 2.into(),
            method: "kernel.status".into(),
            params: serde_json::json!({}),
        },
    )
    .unwrap();
    let status = recv(&mut reader).result.unwrap();
    assert_eq!(status["pid"], child.id());
    assert_eq!(
        status["security"]["bearer_establishes_human_presence"],
        false
    );
    write_frame(
        &mut stream,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 3.into(),
            method: "exec".into(),
            params: serde_json::to_value(ExecParams {
                src: "env.FROM_CONFIG".into(),
                mode: "run".into(),
                position: "value".into(),
                asynchronous: false,
                timeout_ms: None,
                elide: None,
                plan_ref: None,
            })
            .unwrap(),
        },
    )
    .unwrap();
    assert_eq!(
        recv(&mut reader).result.unwrap()["value"],
        serde_json::json!({"$": "str", "v": "config-value"})
    );
    write_frame(
        &mut stream,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 4.into(),
            method: "exec".into(),
            params: serde_json::to_value(ExecParams {
                src: "journal".into(),
                mode: "run".into(),
                position: "value".into(),
                asynchronous: false,
                timeout_ms: None,
                elide: None,
                plan_ref: None,
            })
            .unwrap(),
        },
    )
    .unwrap();
    assert_eq!(
        recv(&mut reader).result.unwrap()["value"],
        serde_json::json!({"$": "table", "cols": {}, "n": 0}),
        "journal.enabled=false disables language-facing statement history"
    );
    write_frame(
        &mut stream,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 5.into(),
            method: "kernel.shutdown".into(),
            params: serde_json::json!({}),
        },
    )
    .unwrap();
    assert_eq!(recv(&mut reader).result.unwrap()["stopping"], true);
    assert!(
        child.wait().unwrap().success(),
        "daemon stderr:\n{}",
        read_daemon_stderr(&stderr_path)
    );
    let audit_rows = Journal::open(&state)
        .unwrap()
        .query(&JournalQuery {
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    assert!(
        audit_rows.iter().any(|row| row.src == "env.FROM_CONFIG"),
        "kernel security/exec audit remains mandatory when language history is disabled"
    );
    assert!(!socket.exists());
}
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn geteuid() -> u32;
    fn dup2(oldfd: i32, newfd: i32) -> i32;
    fn fcntl(fd: i32, command: i32, argument: i32) -> i32;
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
    let state = temp.path().join("state");
    let admin_token = create_admin_token(&state);
    let (stderr_file, stderr_path) = daemon_stderr_file(temp.path());
    let mut child = Command::new(env!("CARGO_BIN_EXE_shoal-kernel"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
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
            params: credentialed_admin_attach_params(&admin_token),
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
    let state = temp.path().join("state");
    let admin_token = create_admin_token(&state);
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
            state.to_str().unwrap(),
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
            params: credentialed_admin_attach_params(&admin_token),
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
