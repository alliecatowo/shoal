//! MCP stdio facade for the shoal kernel protocol.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::net::Shutdown;
use std::os::unix::ffi::OsStrExt;
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

/// Each subscription owns a kernel connection and a forwarding thread. This
/// facade-local ceiling applies before consuming either resource; the kernel's
/// principal/session quotas remain a second, shared admission boundary.
const MAX_FACADE_SUBSCRIPTIONS: usize = 64;
const MAX_AUTOSTART_PATH_BYTES: usize = 4 * 1024;
const MAX_AUTOSTART_SESSION_BYTES: usize = 256;
const MAX_AUTOSTART_NUMBER_BYTES: usize = 20;

fn subscription_admission(active: usize, uri: &str, duplicate: bool) -> Result<bool, String> {
    if uri.len() > resources::MAX_RESOURCE_URI_BYTES {
        return Err(format!(
            "subscription URI is {} bytes; maximum is {}",
            uri.len(),
            resources::MAX_RESOURCE_URI_BYTES
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
            Some(_) => {
                return Some(rpc_error(id, -32601, "method not found", None));
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

    /// Transfer a successfully started kernel to an external supervisor.
    ///
    /// MCP facades deliberately retain their guard so dropping the facade
    /// tears down the daemon it owns. The explicit `shoal kernel start`
    /// command has the opposite lifecycle: after a successful status probe it
    /// exits while the daemon remains alive. Returning the `Child` makes that
    /// ownership transfer explicit and also lets tests retain/reap it.
    pub fn into_child(mut self) -> Option<Child> {
        self.child.take()
    }

    fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let leader_exited = child.try_wait().ok().flatten().is_some();

        // Kill the group even after the leader was reaped: it may have left a
        // same-group worker behind. The unreaped leader prevented pid reuse up
        // to the immediately preceding try_wait; descendants keep the pgid
        // allocated after that point.
        // SAFETY: pgid came from our fresh process-group leader.
        let group_sent = unsafe { libc::kill(-self.pgid, libc::SIGKILL) } == 0;
        if leader_exited {
            return;
        }

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
    start_kernel(config)
}

/// Start a kernel for an explicit supervisor command.
///
/// Unlike [`ensure_kernel`], this ignores `SHOAL_NO_AUTOSTART`: that variable
/// disables implicit MCP startup, not an operator's explicit `kernel start`.
pub fn start_kernel(config: &Config) -> KernelAutostart {
    if UnixStream::connect(&config.socket).is_ok() {
        return KernelAutostart::empty();
    }
    if !autostart_config_admitted(config) {
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
    if let Some(policy) = std::env::var_os("SHOAL_POLICY").filter(|value| {
        !value.is_empty() && value.as_os_str().as_bytes().len() <= MAX_AUTOSTART_PATH_BYTES
    }) {
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
        if let Some(value) = read_env(env).filter(|value| valid_autostart_number(value.as_os_str()))
        {
            cmd.arg(flag).arg(value);
        }
    }
}

fn autostart_config_admitted(config: &Config) -> bool {
    config.socket.as_os_str().as_bytes().len() <= MAX_AUTOSTART_PATH_BYTES
        && config
            .session
            .as_ref()
            .is_none_or(|session| session.len() <= MAX_AUTOSTART_SESSION_BYTES)
}

fn valid_autostart_number(value: &std::ffi::OsStr) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_AUTOSTART_NUMBER_BYTES
        && bytes.iter().all(u8::is_ascii_digit)
        && value
            .to_str()
            .is_some_and(|value| value.parse::<u64>().is_ok())
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
            Err(BridgeError::Protocol(detail)) if is_frame_decode_error(&detail) => {
                write_stdout_frame(&rpc_error(
                    Value::Null,
                    -32700,
                    "parse error",
                    Some(json!({"detail":detail})),
                ))?
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_frame_decode_error(message: &str) -> bool {
    message.starts_with("invalid JSON-RPC")
        || message.starts_with("JSON-RPC frame complexity limit exceeded")
        || message == "JSON-RPC frame exceeds 16 MiB"
}

/// Write one newline-framed JSON value to stdout atomically under the stdout
/// lock. Both the main loop and any subscription forwarder use this, so their
/// frames never interleave on the wire.
pub(crate) fn write_stdout_frame(value: &Value) -> Result<(), BridgeError> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    shoal_proto::write_frame(&mut handle, value).map_err(BridgeError::Io)
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
    shoal_proto::read_json_frame(reader).map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidData {
            BridgeError::Protocol(error.to_string())
        } else {
            BridgeError::Io(error)
        }
    })
}
pub(crate) fn write_json_line<W: Write>(writer: &mut W, value: &Value) -> Result<(), BridgeError> {
    shoal_proto::write_frame(writer, value).map_err(BridgeError::Io)
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
    use shoal_proto::MAX_FRAME_LEN;
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
        let mut body = "x".repeat(MAX_FRAME_LEN + 1024);
        body.push('\n');
        let mut reader = io::BufReader::new(body.as_bytes());
        let error = read_json_line(&mut reader).expect_err("an oversized frame must fail");
        match error {
            BridgeError::Protocol(message) => assert!(message.contains("16 MiB"), "{message}"),
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[test]
    fn facade_frame_io_shares_complexity_limits_and_recovers_at_line_boundary() {
        let wide = format!(
            "[{}]",
            std::iter::repeat_n("null", shoal_proto::MAX_JSON_CONTAINER_ITEMS + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        let good = json!({"jsonrpc":"2.0","id":1,"method":"ping"});
        let mut bytes = wide.into_bytes();
        bytes.push(b'\n');
        bytes.extend_from_slice(serde_json::to_string(&good).unwrap().as_bytes());
        bytes.push(b'\n');
        let mut reader = io::BufReader::new(bytes.as_slice());
        let error = read_json_line(&mut reader).unwrap_err();
        match error {
            BridgeError::Protocol(message) => assert!(message.contains("complexity limit")),
            other => panic!("expected a protocol error, got {other:?}"),
        }
        assert_eq!(read_json_line(&mut reader).unwrap(), Some(good));

        let mut output = Vec::new();
        let wide = Value::Array(vec![Value::Null; shoal_proto::MAX_JSON_CONTAINER_ITEMS + 1]);
        assert!(write_json_line(&mut output, &wide).is_err());
        assert!(output.is_empty());
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
    fn explicit_supervisor_can_transfer_child_without_drop_killing_it() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]).process_group(0);
        let child = command.spawn().unwrap();
        let pid = child.id();
        let guard = KernelAutostart::new(child);

        let mut child = guard.into_child().expect("spawned child transfers");
        assert!(!process_is_gone(pid));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(process_is_gone(pid));
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
    fn exited_kernel_leader_cannot_leave_group_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let descendant_path = dir.path().join("descendant.pid");
        let script = format!(
            "sleep 30 & echo $! > '{}'; exit 0",
            descendant_path.display()
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]).process_group(0);
        let child = command.spawn().unwrap();
        let mut guard = KernelAutostart::new(child);
        let deadline = Instant::now() + Duration::from_secs(1);
        while guard.child.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "leader did not exit");
            thread::sleep(Duration::from_millis(5));
        }
        let descendant = std::fs::read_to_string(descendant_path)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert!(!process_is_gone(descendant));
        drop(guard);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !process_is_gone(descendant) {
            assert!(Instant::now() < deadline, "descendant survived guard drop");
            thread::sleep(Duration::from_millis(5));
        }
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
    fn autostart_rejects_environment_to_argv_amplification() {
        for invalid in [
            std::ffi::OsString::from("-1"),
            std::ffi::OsString::from("1x"),
            std::ffi::OsString::from("9".repeat(MAX_AUTOSTART_NUMBER_BYTES + 1)),
            std::ffi::OsString::from("18446744073709551616"),
        ] {
            let mut command = Command::new("shoal-kernel");
            append_kernel_limit_args(&mut command, |name| {
                (name == "SHOAL_MAX_SESSIONS").then(|| invalid.clone())
            });
            assert!(command.get_args().next().is_none());
        }

        let config = Config {
            socket: PathBuf::from("/tmp/kernel.sock"),
            session: Some("s".repeat(MAX_AUTOSTART_SESSION_BYTES + 1)),
            token: None,
            local_auth: LocalAuthMode::RestrictedAgent,
        };
        assert!(!autostart_config_admitted(&config));
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
                &format!(
                    "shoal://events/{}",
                    "x".repeat(resources::MAX_RESOURCE_URI_BYTES)
                ),
                false,
            )
            .unwrap_err()
            .contains("subscription URI")
        );
    }
}
