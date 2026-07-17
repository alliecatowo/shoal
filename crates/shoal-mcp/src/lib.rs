//! MCP stdio facade for the shoal kernel protocol.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod client;
mod resources;
mod tools;

pub use client::{BridgeError, Config, KernelClient, LocalAuthMode, discover_socket};
pub use tools::tools;

const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Each subscription owns a kernel connection and a forwarding thread. This
/// facade-local ceiling applies before consuming either resource; the kernel's
/// principal/session quotas remain a second, shared admission boundary.
const MAX_FACADE_SUBSCRIPTIONS: usize = 64;
const MAX_SUBSCRIPTION_URI_BYTES: usize = 4 * 1024;

fn subscription_admission(active: usize, uri: &str, duplicate: bool) -> Result<bool, String> {
    if uri.len() > MAX_SUBSCRIPTION_URI_BYTES {
        return Err(format!(
            "subscription URI is {} bytes; maximum is {MAX_SUBSCRIPTION_URI_BYTES}",
            uri.len()
        ));
    }
    if duplicate {
        return Ok(false);
    }
    if active >= MAX_FACADE_SUBSCRIPTIONS {
        return Err(format!(
            "MCP facade subscription limit ({MAX_FACADE_SUBSCRIPTIONS}) reached; unsubscribe before adding another resource"
        ));
    }
    Ok(true)
}

pub struct Facade {
    kernel: KernelClient,
    config: Config,
    subscriptions: HashMap<String, SubscriptionWorker>,
    // Keeps exactly the daemon this facade autostarted alive and owned. Drop
    // terminates/reaps it after the facade's protocol connections are gone.
    _autostart: KernelAutostart,
}

struct SubscriptionWorker {
    interrupt: UnixStream,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SubscriptionWorker {
    fn drop(&mut self) {
        let _ = self.interrupt.shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Facade {
    pub fn connect(config: &Config) -> Result<Self, BridgeError> {
        Self::connect_with_autostart(config, KernelAutostart::empty())
    }

    fn connect_with_autostart(
        config: &Config,
        autostart: KernelAutostart,
    ) -> Result<Self, BridgeError> {
        Ok(Self {
            kernel: KernelClient::connect(config)?,
            config: config.clone(),
            subscriptions: HashMap::new(),
            _autostart: autostart,
        })
    }
    pub fn handle(&mut self, request: &Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str);
        // MCP notifications intentionally have no response.
        let id = id?;
        if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(rpc_error(id, -32600, "invalid JSON-RPC request", None));
        }
        let result = match method {
            Some("initialize") => Ok(
                json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false},"resources":{"subscribe":true,"listChanged":false}},"serverInfo":{"name":"shoal-mcp","version":env!("CARGO_PKG_VERSION")}}),
            ),
            Some("ping") => Ok(json!({})),
            Some("tools/list") => Ok(json!({"tools":tools()})),
            Some("tools/call") => {
                self.tools_call(request.get("params").cloned().unwrap_or(Value::Null))
            }
            Some("resources/list") => self.resources_list(),
            Some("resources/templates/list") => Ok(resources::resource_templates()),
            Some("resources/read") => {
                self.resources_read(request.get("params").cloned().unwrap_or(Value::Null))
            }
            Some("resources/subscribe") => {
                self.resources_subscribe(request.get("params").cloned().unwrap_or(Value::Null))
            }
            Some("resources/unsubscribe") => {
                self.resources_unsubscribe(request.get("params").cloned().unwrap_or(Value::Null))
            }
            Some(m) => {
                return Some(rpc_error(
                    id,
                    -32601,
                    "method not found",
                    Some(json!({"method":m})),
                ));
            }
            None => {
                return Some(rpc_error(
                    id,
                    -32600,
                    "request method must be a string",
                    None,
                ));
            }
        };
        Some(match result {
            Ok(v) => json!({"jsonrpc":"2.0","id":id,"result":v}),
            Err(e) => rpc_error(id, -32602, &e, None),
        })
    }

    pub fn active_subscriptions(&self) -> usize {
        self.subscriptions.len()
    }
}

/// Ownership guard for a kernel process spawned by this MCP facade.
///
/// An empty guard represents an independently managed listener. A populated
/// guard kills the spawned process group (and direct leader as a fallback)
/// and reaps the leader on drop. Keeping it inside [`Facade`] prevents both
/// premature daemon loss and abandoned child/zombie processes.
pub struct KernelAutostart {
    child: Option<Child>,
    pgid: libc::pid_t,
}

impl KernelAutostart {
    fn empty() -> Self {
        Self {
            child: None,
            pgid: 0,
        }
    }

    fn new(child: Child) -> Self {
        let pgid = child.id() as libc::pid_t;
        Self {
            child: Some(child),
            pgid,
        }
    }

    fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_some() {
            return;
        }

        // SAFETY: pgid is the positive pid of the child placed in a fresh
        // process group by `kernel_command` / `start_kernel_command`.
        let group_sent = unsafe { libc::kill(-self.pgid, libc::SIGKILL) } == 0;
        let leader_sent = child.kill().is_ok();
        if group_sent || leader_sent {
            let _ = child.wait();
        } else {
            // If the OS refused both kill routes, do not replace a bounded
            // shutdown with an unbounded wait. This still reaps an exit that
            // raced the signals.
            let _ = child.try_wait();
        }
    }

    #[cfg(test)]
    fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }
}

impl Drop for KernelAutostart {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Best-effort: make sure a kernel is listening on `config.socket`, lazily
/// bringing up a detached `shoal-kernel` daemon if none is. This is what makes
/// the MCP plugin zero-config — registering `shoal mcp` as an MCP server is the
/// whole setup; the first agent connection spawns the per-user kernel if it
/// isn't already running, instead of failing with a bare "connection refused".
///
/// Never fails the caller: if the kernel can't be started (binary not on
/// `PATH`, exec trouble) the subsequent `Facade::connect` surfaces the real
/// connection error. Opt out with `SHOAL_NO_AUTOSTART=1` when the kernel is
/// supervised externally (a systemd user unit, a hand-started daemon).
///
/// The kernel's own `prepare_socket` refuses to bind a socket another kernel is
/// already listening on, so two agents racing to autostart just leave one live
/// daemon — we connect to whichever won.
pub fn ensure_kernel(config: &Config) -> KernelAutostart {
    // Warm-daemon fast path: a listener is already up, nothing to do.
    if UnixStream::connect(&config.socket).is_ok() {
        return KernelAutostart::empty();
    }
    if std::env::var_os("SHOAL_NO_AUTOSTART").is_some_and(|v| !v.is_empty()) {
        return KernelAutostart::empty();
    }
    let program = kernel_program();
    let mut cmd = kernel_command(config, &program);
    start_kernel_command(
        config,
        &mut cmd,
        Duration::from_secs(5),
        Duration::from_millis(50),
    )
}

fn start_kernel_command(
    config: &Config,
    command: &mut Command,
    readiness_timeout: Duration,
    poll_interval: Duration,
) -> KernelAutostart {
    // Enforce the ownership boundary here too so test/custom commands cannot
    // accidentally bypass group cleanup.
    command.process_group(0);
    let Ok(child) = command.spawn() else {
        // Not on PATH / cannot exec — Facade::connect surfaces the real error.
        return KernelAutostart::empty();
    };
    let mut ownership = KernelAutostart::new(child);
    let start = Instant::now();
    loop {
        if UnixStream::connect(&config.socket).is_ok() {
            return ownership;
        }
        let child_state = ownership.child.as_mut().map(std::process::Child::try_wait);
        match child_state {
            Some(Ok(Some(_))) | Some(Err(_)) | None => {
                ownership.terminate();
                return KernelAutostart::empty();
            }
            Some(Ok(None)) => {}
        }
        if start.elapsed() >= readiness_timeout {
            ownership.terminate();
            return KernelAutostart::empty();
        }
        std::thread::sleep(poll_interval.min(readiness_timeout.saturating_sub(start.elapsed())));
    }
}

/// Prefer the kernel installed beside this MCP facade. Besides making bundled
/// installs reliable, this avoids letting a workspace-leading `PATH` entry
/// substitute a different binary whenever the trusted sibling is available.
/// Development/source layouts still fall back to normal PATH discovery.
fn kernel_program() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| sibling_kernel(&exe))
        .unwrap_or_else(|| PathBuf::from("shoal-kernel"))
}

fn sibling_kernel(current_exe: &Path) -> Option<PathBuf> {
    let sibling = current_exe.parent()?.join("shoal-kernel");
    sibling.is_file().then_some(sibling)
}

fn kernel_command(config: &Config, program: &Path) -> Command {
    let mut cmd = Command::new(program);
    let paths = shoal_paths::ShoalPaths::discover();
    cmd.arg("--socket")
        .arg(&config.socket)
        .arg("--state-dir")
        .arg(paths.state_dir());
    if let Some(session) = &config.session {
        cmd.arg("--session").arg(session);
    }
    if let Some(policy) = std::env::var_os("SHOAL_POLICY").filter(|value| !value.is_empty()) {
        cmd.arg("--policy").arg(policy);
    }
    append_kernel_limit_args(&mut cmd, |name| std::env::var_os(name));
    cmd
        // The facade already captured the bearer for its own attach request.
        // The daemon neither needs nor should inherit that secret: a kernel
        // evaluator and its child processes inherit the daemon environment.
        .env_remove("SHOAL_TOKEN")
        // Detach stdout/stdin and start a new process group so the owning
        // facade can terminate the whole autostarted tree without touching
        // itself or an independently supervised kernel.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Keep stderr inherited: startup/policy failures must remain diagnosable
        // instead of becoming a bare connection-refused five seconds later.
        .stderr(Stdio::inherit())
        .process_group(0);
    cmd
}

fn append_kernel_limit_args(
    cmd: &mut Command,
    read_env: impl Fn(&str) -> Option<std::ffi::OsString>,
) {
    for (env, flag) in [
        ("SHOAL_MAX_CONNECTIONS", "--max-connections"),
        ("SHOAL_MAX_SESSIONS", "--max-sessions"),
        ("SHOAL_MAX_TASKS_PER_SESSION", "--max-tasks-per-session"),
        ("SHOAL_MAX_PTYS_PER_SESSION", "--max-ptys-per-session"),
        ("SHOAL_MAX_PTYS_PER_PRINCIPAL", "--max-ptys-per-principal"),
        ("SHOAL_MAX_PTYS_GLOBAL", "--max-ptys-global"),
        (
            "SHOAL_MAX_SUBSCRIPTIONS_PER_SESSION",
            "--max-subscriptions-per-session",
        ),
        ("SHOAL_FRAME_READ_TIMEOUT_MS", "--frame-read-timeout-ms"),
    ] {
        if let Some(value) = read_env(env).filter(|value| !value.is_empty()) {
            cmd.arg(flag).arg(value);
        }
    }
}

pub fn run_stdio(config: &Config) -> Result<(), BridgeError> {
    let autostart = ensure_kernel(config);
    let mut facade = Facade::connect_with_autostart(config, autostart)?;
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    // stdout is written per-frame under its own lock (via `write_stdout_frame`)
    // rather than held for the whole loop, so a subscription forwarder thread
    // can also push notification frames without deadlocking on the writer.
    loop {
        match read_json_line(&mut reader) {
            Ok(Some(request)) => {
                if let Some(response) = facade.handle(&request) {
                    write_stdout_frame(&response)?
                }
            }
            Ok(None) => return Ok(()),
            Err(BridgeError::Json(error)) => write_stdout_frame(&rpc_error(
                Value::Null,
                -32700,
                "parse error",
                Some(json!({"detail":error.to_string()})),
            ))?,
            Err(error) => return Err(error),
        }
    }
}

/// Write one newline-framed JSON value to stdout atomically under the stdout
/// lock. Both the main loop and any subscription forwarder use this, so their
/// frames never interleave on the wire.
pub(crate) fn write_stdout_frame(value: &Value) -> Result<(), BridgeError> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(&buf)?;
    handle.flush()?;
    Ok(())
}

fn rpc_error(id: Value, code: i32, message: &str, data: Option<Value>) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message,"data":data}})
}

/// `kind:id` short ref → `shoal://kind/id` URI (site/content/internals/kernel-protocol.md).
pub(crate) fn short_ref_to_uri(short: &str) -> String {
    match short.split_once(':') {
        Some((kind, rest)) => format!("shoal://{kind}/{rest}"),
        None => format!("shoal://{short}"),
    }
}

pub(crate) fn read_json_line<R: BufRead>(reader: &mut R) -> Result<Option<Value>, BridgeError> {
    let mut line = String::new();
    let n = reader
        .by_ref()
        .take(MAX_FRAME as u64 + 1)
        .read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_FRAME {
        return Err(BridgeError::Protocol("frame exceeds 16 MiB".into()));
    }
    Ok(Some(serde_json::from_str(line.trim_end())?))
}
pub(crate) fn write_json_line<W: Write>(writer: &mut W, value: &Value) -> Result<(), BridgeError> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub fn socket_exists(path: &Path) -> bool {
    fs_type(path).is_some_and(|t| t.is_socket())
}

fn fs_type(path: &Path) -> Option<std::fs::FileType> {
    std::fs::metadata(path).ok().map(|m| m.file_type())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::thread;

    fn process_is_gone(pid: u32) -> bool {
        // SAFETY: signal 0 only checks whether the recorded process exists.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
    fn mock() -> (tempfile::TempDir, Config, thread::JoinHandle<Vec<Value>>) {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("kernel.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let h = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut r = io::BufReader::new(stream.try_clone().unwrap());
            let mut w = stream;
            let mut seen = vec![];
            for n in 0..2 {
                let req = read_json_line(&mut r).unwrap().unwrap();
                seen.push(req.clone());
                let id = req["id"].clone();
                let result = if n == 0 {
                    json!({"session":"s","principal":"human","caps":{},"cwd":{"display":"/tmp"},"env_hash":"x","ast_version":1})
                } else {
                    json!({"ref":"out:1","value":{"$":"int","v":3}})
                };
                write_json_line(&mut w, &json!({"jsonrpc":"2.0","id":id,"result":result})).unwrap()
            }
            seen
        });
        let c = Config {
            socket: path,
            session: Some("s".into()),
            token: Some("tok".into()),
            local_auth: LocalAuthMode::RestrictedAgent,
        };
        (d, c, h)
    }

    #[test]
    fn read_json_line_rejects_unterminated_unbounded_input() {
        struct Infinite;

        impl io::Read for Infinite {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                buf.fill(b'x');
                Ok(buf.len())
            }
        }

        let mut reader = io::BufReader::new(Infinite);
        let error = read_json_line(&mut reader)
            .expect_err("an unterminated oversized frame must fail without unbounded buffering");
        match error {
            BridgeError::Protocol(message) => assert!(message.contains("16 MiB"), "{message}"),
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[test]
    fn read_json_line_rejects_a_single_oversized_line() {
        let mut body = "x".repeat(MAX_FRAME + 1024);
        body.push('\n');
        let mut reader = io::BufReader::new(body.as_bytes());
        let error = read_json_line(&mut reader).expect_err("an oversized frame must fail");
        match error {
            BridgeError::Protocol(message) => assert!(message.contains("16 MiB"), "{message}"),
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[test]
    fn facade_attaches_and_maps_exec() {
        let (_d, c, h) = mock();
        let mut f = Facade::connect(&c).unwrap();
        let response=f.handle(&json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"shoal_exec","arguments":{"src":"1+2"}}})).unwrap();
        assert_eq!(response["result"]["structuredContent"]["ref"], "out:1");
        let seen = h.join().unwrap();
        assert_eq!(seen[0]["method"], "session.attach");
        assert_eq!(seen[0]["params"]["token"], "tok");
        assert_eq!(seen[1]["method"], "exec");
        assert_eq!(seen[1]["params"]["mode"], "run");
    }
    #[test]
    fn protocol_errors_are_structured() {
        let (_d, c, h) = mock();
        let mut f = Facade::connect(&c).unwrap();
        let e = f
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"nope"}))
            .unwrap();
        assert_eq!(e["error"]["code"], -32601);
        drop(f);
        let _ = h.join();
    }
    #[test]
    fn socket_probe_is_truthful() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        assert!(!socket_exists(&p));
        let _l = UnixListener::bind(&p).unwrap();
        assert!(socket_exists(&p));
    }

    /// Warm-daemon fast path: when a kernel is already listening, `ensure_kernel`
    /// returns at once via the connect probe — it must not spawn anything or
    /// block. (The real spawn path needs `shoal-kernel` on `PATH`, so it's
    /// covered by out-of-process dogfooding, not this in-process test.)
    #[test]
    fn ensure_kernel_is_a_noop_when_a_listener_is_up() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("kernel.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let c = Config {
            socket: path,
            session: None,
            token: None,
            local_auth: LocalAuthMode::RestrictedAgent,
        };
        // Returns immediately because the connect probe succeeds; a hang or a
        // stray spawn would show up as a test timeout / leaked process.
        let guard = ensure_kernel(&c);
        assert_eq!(guard.pid(), None);
        drop(guard);
        assert!(UnixStream::connect(&c.socket).is_ok());
    }

    #[test]
    fn autostart_readiness_timeout_kills_and_reaps_owned_child() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("kernel.pid");
        let config = Config {
            socket: dir.path().join("never-ready.sock"),
            session: None,
            token: None,
            local_auth: LocalAuthMode::RestrictedAgent,
        };
        let script = format!("echo $$ > '{}'; sleep 30", pid_path.display());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]);

        let start = Instant::now();
        let guard = start_kernel_command(
            &config,
            &mut command,
            Duration::from_millis(80),
            Duration::from_millis(5),
        );
        assert_eq!(guard.pid(), None);
        assert!(start.elapsed() < Duration::from_secs(1));
        let pid = std::fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert!(process_is_gone(pid));
    }

    #[test]
    fn ready_autostart_remains_owned_until_guard_drop() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ready.sock");
        let config = Config {
            socket: socket.clone(),
            session: None,
            token: None,
            local_auth: LocalAuthMode::RestrictedAgent,
        };
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let listener_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            let listener = UnixListener::bind(socket).unwrap();
            ready_tx.send(listener).unwrap();
        });
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let guard = start_kernel_command(
            &config,
            &mut command,
            Duration::from_secs(1),
            Duration::from_millis(5),
        );
        let listener = ready_rx.recv().unwrap();
        listener_thread.join().unwrap();
        let pid = guard.pid().expect("ready child remains owned");
        assert!(!process_is_gone(pid));
        drop(guard);
        assert!(process_is_gone(pid));
        drop(listener);
    }

    #[test]
    fn facade_retains_then_reaps_its_autostarted_kernel() {
        let (_dir, config, server) = mock();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]).process_group(0);
        let child = command.spawn().unwrap();
        let pid = child.id();
        let guard = KernelAutostart::new(child);

        let mut facade = Facade::connect_with_autostart(&config, guard).unwrap();
        assert!(!process_is_gone(pid));
        let _ = facade.handle(&json!({
            "jsonrpc":"2.0",
            "id":7,
            "method":"tools/call",
            "params":{"name":"shoal_exec","arguments":{"src":"1+2"}}
        }));
        drop(facade);
        assert!(process_is_gone(pid));
        server.join().unwrap();
    }

    #[test]
    fn autostart_prefers_a_sibling_kernel_and_scrubs_the_bearer() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("shoal-mcp");
        let sibling = dir.path().join("shoal-kernel");
        std::fs::write(&current, b"").unwrap();
        std::fs::write(&sibling, b"").unwrap();
        assert_eq!(sibling_kernel(&current), Some(sibling.clone()));

        let config = Config {
            socket: dir.path().join("kernel.sock"),
            session: Some("test".into()),
            token: Some("must-not-reach-kernel".into()),
            local_auth: LocalAuthMode::RestrictedAgent,
        };
        let command = kernel_command(&config, &sibling);
        assert_eq!(command.get_program(), sibling.as_os_str());
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["--session", "test"]));
        assert!(args.iter().any(|arg| arg == "--state-dir"));
        assert!(
            command.get_envs().any(|(key, value)| {
                key == std::ffi::OsStr::new("SHOAL_TOKEN") && value.is_none()
            })
        );
    }

    #[test]
    fn autostart_forwards_the_global_session_limit() {
        let mut command = Command::new("shoal-kernel");
        append_kernel_limit_args(&mut command, |name| {
            (name == "SHOAL_MAX_SESSIONS").then(|| std::ffi::OsString::from("19"))
        });
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args, ["--max-sessions", "19"]);
    }

    #[test]
    fn subscription_admission_is_bounded_and_duplicate_idempotent() {
        assert!(
            !subscription_admission(MAX_FACADE_SUBSCRIPTIONS, "shoal://events/same", true).unwrap()
        );
        let error = subscription_admission(MAX_FACADE_SUBSCRIPTIONS, "shoal://events/new", false)
            .unwrap_err();
        assert!(error.contains("unsubscribe"));
        assert!(
            subscription_admission(
                0,
                &format!("shoal://events/{}", "x".repeat(MAX_SUBSCRIPTION_URI_BYTES)),
                false,
            )
            .unwrap_err()
            .contains("subscription URI")
        );
    }
}
