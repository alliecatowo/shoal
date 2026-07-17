//! MCP stdio facade for the shoal kernel protocol.

use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod client;
mod resources;
mod tools;

pub use client::{BridgeError, Config, KernelClient, LocalAuthMode, discover_socket};
pub use tools::tools;

const MAX_FRAME: usize = 16 * 1024 * 1024;

pub struct Facade {
    kernel: KernelClient,
    config: Config,
}
impl Facade {
    pub fn connect(config: &Config) -> Result<Self, BridgeError> {
        Ok(Self {
            kernel: KernelClient::connect(config)?,
            config: config.clone(),
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
            Some("resources/unsubscribe") => Ok(json!({})),
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
pub fn ensure_kernel(config: &Config) {
    // Warm-daemon fast path: a listener is already up, nothing to do.
    if UnixStream::connect(&config.socket).is_ok() {
        return;
    }
    if std::env::var_os("SHOAL_NO_AUTOSTART").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let program = kernel_program();
    let mut cmd = kernel_command(config, &program);
    if cmd.spawn().is_err() {
        return; // not on PATH / cannot exec — let Facade::connect surface it
    }
    // Poll for readiness (kernel binds its socket, then accepts). Bounded at
    // ~5s so a genuinely broken kernel can't hang the agent forever.
    for _ in 0..100 {
        if UnixStream::connect(&config.socket).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
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
    cmd.arg("--socket")
        .arg(&config.socket)
        // The facade already captured the bearer for its own attach request.
        // The daemon neither needs nor should inherit that secret: a kernel
        // evaluator and its child processes inherit the daemon environment.
        .env_remove("SHOAL_TOKEN")
        // Detach: silence stdio and start a new process group so the daemon
        // outlives this (per-session, per-agent) mcp process.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    cmd
}

pub fn run_stdio(config: &Config) -> Result<(), BridgeError> {
    ensure_kernel(config);
    let mut facade = Facade::connect(config)?;
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
    use std::thread;
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
        ensure_kernel(&c);
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
            session: None,
            token: Some("must-not-reach-kernel".into()),
            local_auth: LocalAuthMode::RestrictedAgent,
        };
        let command = kernel_command(&config, &sibling);
        assert_eq!(command.get_program(), sibling.as_os_str());
        assert!(
            command.get_envs().any(|(key, value)| {
                key == std::ffi::OsStr::new("SHOAL_TOKEN") && value.is_none()
            })
        );
    }
}
