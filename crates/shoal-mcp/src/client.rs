//! Kernel connection: `Config`, Unix-socket discovery, and the JSON-RPC
//! `KernelClient` used to talk to `shoal-kernel` over its Unix socket.

use crate::{read_json_line, write_json_line, write_stdout_frame};
use serde_json::{Value, json};
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub socket: PathBuf,
    pub session: Option<String>,
    pub token: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let session = std::env::var("SHOAL_SESSION").ok();
        let socket = discover_socket(session.as_deref().unwrap_or("default"));
        Ok(Self {
            socket,
            session,
            token: std::env::var("SHOAL_TOKEN").ok(),
        })
    }
}

/// Resolve the kernel socket the SAME way `shoal-kernel` does, so discovery
/// works cross-platform — in particular on macOS, where `XDG_RUNTIME_DIR` is
/// unset by default and the kernel falls back to `/tmp/shoal-{uid}`. Order:
///
/// 1. `SHOAL_SOCKET` (explicit override) — used verbatim.
/// 2. `$XDG_RUNTIME_DIR/shoal/{session}.sock`.
/// 3. `$TMPDIR/shoal-{uid}/shoal/{session}.sock` (macOS sets `TMPDIR`).
/// 4. `/tmp/shoal-{uid}/shoal/{session}.sock` (kernel's own final fallback).
///
/// Without this, a bare `XDG_RUNTIME_DIR`-only lookup silently failed on macOS
/// and socket discovery never found the running kernel.
pub fn discover_socket(session: &str) -> PathBuf {
    if let Some(explicit) = std::env::var_os("SHOAL_SOCKET").filter(|s| !s.is_empty()) {
        return PathBuf::from(explicit);
    }
    runtime_dir().join("shoal").join(format!("{session}.sock"))
}

/// The runtime directory the kernel binds its socket under. Mirrors
/// `shoal-kernel`'s `runtime_socket`, with a `$TMPDIR` step so a macOS session
/// that exports `TMPDIR` (but not `XDG_RUNTIME_DIR`) is honored before the
/// hard `/tmp/shoal-{uid}` fallback.
fn runtime_dir() -> PathBuf {
    runtime_dir_from(
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("TMPDIR"),
        unsafe { geteuid() },
    )
}

/// Pure socket-directory selection (kept separate so the macOS no-`XDG` case is
/// unit-testable without mutating process env): `$XDG_RUNTIME_DIR`, else
/// `$TMPDIR/shoal-{uid}`, else `/tmp/shoal-{uid}` — identical to shoal-kernel.
fn runtime_dir_from(
    xdg: Option<std::ffi::OsString>,
    tmpdir: Option<std::ffi::OsString>,
    uid: u32,
) -> PathBuf {
    if let Some(xdg) = xdg.filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg);
    }
    if let Some(tmp) = tmpdir.filter(|s| !s.is_empty()) {
        return PathBuf::from(tmp).join(format!("shoal-{uid}"));
    }
    PathBuf::from(format!("/tmp/shoal-{uid}"))
}

unsafe extern "C" {
    fn geteuid() -> u32;
}

pub struct KernelClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    pub(crate) attach: Value,
}

impl KernelClient {
    pub fn connect(config: &Config) -> Result<Self, BridgeError> {
        let stream = UnixStream::connect(&config.socket)?;
        let mut client = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_id: 1,
            attach: Value::Null,
        };
        client.attach = client.call(
            "session.attach",
            json!({
                "session": config.session,
                "token": config.token,
                "client": {"kind":"mcp", "tty":false}
            }),
        )?;
        Ok(client)
    }

    /// Subscribe on this (dedicated) connection and forward every pushed
    /// `event` notification to MCP stdout as `notifications/resources/updated`
    /// (site/content/internals/kernel-protocol.md). Runs until the connection closes.
    pub(crate) fn run_event_forwarder(mut self, channel: String, uri: String) {
        if self
            .call("events.subscribe", json!({"channel": channel}))
            .is_err()
        {
            return;
        }
        while let Ok(Some(frame)) = read_json_line(&mut self.reader) {
            if frame.get("method").and_then(Value::as_str) == Some("event") {
                let p = frame.get("params").cloned().unwrap_or(Value::Null);
                let note = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/resources/updated",
                    "params": {
                        "uri": uri,
                        "seq": p.get("seq"),
                        "payload": p.get("payload"),
                    }
                });
                let _ = write_stdout_frame(&note);
            }
        }
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, BridgeError> {
        let id = self.next_id;
        self.next_id += 1;
        write_json_line(
            &mut self.writer,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        loop {
            let frame = read_json_line(&mut self.reader)?.ok_or(BridgeError::Disconnected)?;
            // Kernel notifications can be interleaved with the response.
            if frame.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = frame.get("error") {
                return Err(BridgeError::Kernel(error.clone()));
            }
            return frame.get("result").cloned().ok_or_else(|| {
                BridgeError::Protocol("kernel response has neither result nor error".into())
            });
        }
    }
}

#[derive(Debug)]
pub enum BridgeError {
    Io(io::Error),
    Json(serde_json::Error),
    Protocol(String),
    Kernel(Value),
    Disconnected,
}
impl From<io::Error> for BridgeError {
    fn from(v: io::Error) -> Self {
        Self::Io(v)
    }
}
impl From<serde_json::Error> for BridgeError {
    fn from(v: serde_json::Error) -> Self {
        Self::Json(v)
    }
}
impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "{e}"),
            Self::Protocol(e) => write!(f, "{e}"),
            Self::Kernel(e) => write!(f, "kernel error: {e}"),
            Self::Disconnected => write!(f, "kernel disconnected"),
        }
    }
}
impl std::error::Error for BridgeError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// macOS-first-class socket discovery: with no `XDG_RUNTIME_DIR` (the macOS
    /// default), the path must fall through exactly as shoal-kernel does — to
    /// `$TMPDIR/shoal-{uid}` when `TMPDIR` is set, else `/tmp/shoal-{uid}`.
    #[test]
    fn socket_discovery_falls_back_without_xdg() {
        use std::ffi::OsString;
        // No XDG, no TMPDIR → hard /tmp fallback.
        assert_eq!(
            runtime_dir_from(None, None, 501),
            PathBuf::from("/tmp/shoal-501")
        );
        // No XDG, TMPDIR set (the macOS shape) → $TMPDIR/shoal-{uid}.
        assert_eq!(
            runtime_dir_from(None, Some(OsString::from("/var/folders/xy")), 501),
            PathBuf::from("/var/folders/xy/shoal-501")
        );
        // XDG present → used verbatim (Linux).
        assert_eq!(
            runtime_dir_from(
                Some(OsString::from("/run/user/1000")),
                Some(OsString::from("/tmp")),
                1000
            ),
            PathBuf::from("/run/user/1000")
        );
        // Empty XDG is treated as unset (a common shell footgun).
        assert_eq!(
            runtime_dir_from(Some(OsString::new()), None, 7),
            PathBuf::from("/tmp/shoal-7")
        );
    }
}
