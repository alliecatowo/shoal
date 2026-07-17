//! Kernel connection: `Config`, Unix-socket discovery, and the JSON-RPC
//! `KernelClient` used to talk to `shoal-kernel` over its Unix socket.

use crate::{read_json_line, write_json_line, write_stdout_frame};
use serde_json::{Value, json};
pub use shoal_proto::LocalAuthMode;
use shoal_proto::{ATTACH_SECURITY_EPOCH, PRINCIPAL_SESSION_ISOLATION};
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub socket: PathBuf,
    pub session: Option<String>,
    pub token: Option<String>,
    pub local_auth: LocalAuthMode,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let session = std::env::var("SHOAL_SESSION").ok();
        let socket = discover_socket(session.as_deref().unwrap_or("default"));
        Ok(Self {
            socket,
            session,
            token: std::env::var("SHOAL_TOKEN").ok(),
            local_auth: LocalAuthMode::RestrictedAgent,
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
    shoal_paths::ShoalPaths::discover().socket(session)
}

pub struct KernelClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    pub(crate) attach: Value,
}

impl KernelClient {
    pub fn connect(config: &Config) -> Result<Self, BridgeError> {
        let params = attach_params(config)?;
        let stream = UnixStream::connect(&config.socket)?;
        let mut client = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_id: 1,
            attach: Value::Null,
        };
        client.attach = client.call("session.attach", params)?;
        validate_attach_security(config, &client.attach)?;
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

fn attach_params(config: &Config) -> Result<Value, BridgeError> {
    if config.token.is_some() && config.local_auth == LocalAuthMode::LocalHuman {
        return Err(BridgeError::Protocol(
            "--token and --local-human are mutually exclusive authentication modes".into(),
        ));
    }
    let mut params = json!({
        "session": config.session,
        "token": config.token,
        "client": {"kind":"mcp", "tty":false}
    });
    if config.token.is_none() {
        params["local_auth"] = serde_json::to_value(config.local_auth)?;
    }
    Ok(params)
}

/// Refuse a silent security downgrade when a zero-token MCP asks for the
/// restricted local-agent boundary but reaches a kernel that ignores the new
/// attach field and grants the historical unrestricted local-human identity.
///
/// Explicit local-human mode intentionally accepts the legacy response: the
/// user already opted into exactly that permissive boundary. Bearer auth keeps
/// its existing compatibility until the kernel's principal-session migration
/// is complete; the token still selects its configured principal.
fn validate_attach_security(config: &Config, attach: &Value) -> Result<(), BridgeError> {
    if config.token.is_some() {
        return Ok(());
    }
    match config.local_auth {
        LocalAuthMode::LocalHuman => {
            if let Some(mode) = attach.get("auth_mode").and_then(Value::as_str)
                && mode != "local-human"
            {
                return Err(BridgeError::Protocol(format!(
                    "kernel attached with auth_mode {mode:?}, not requested local-human"
                )));
            }
            Ok(())
        }
        LocalAuthMode::RestrictedAgent => {
            let mode = attach.get("auth_mode").and_then(Value::as_str);
            let isolation = attach.get("session_isolation").and_then(Value::as_str);
            let epoch = attach.get("security_epoch").and_then(Value::as_u64);
            let principal = attach.get("principal").and_then(Value::as_str);
            if mode != Some("restricted-agent")
                || isolation != Some(PRINCIPAL_SESSION_ISOLATION)
                || epoch.is_none_or(|v| v < u64::from(ATTACH_SECURITY_EPOCH))
                || !principal.is_some_and(|p| p.starts_with("agent:"))
            {
                return Err(BridgeError::Protocol(
                    "kernel cannot prove restricted MCP attach and principal-isolated sessions; \
                     upgrade shoal-kernel, provide a bearer token, or explicitly opt into \
                     --local-human"
                        .into(),
                ));
            }
            Ok(())
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

    fn config(local_auth: LocalAuthMode, token: Option<&str>) -> Config {
        Config {
            socket: PathBuf::from("/tmp/not-used.sock"),
            session: Some("test".into()),
            token: token.map(str::to_owned),
            local_auth,
        }
    }

    #[test]
    fn restricted_attach_requires_hardened_kernel_metadata() {
        let config = config(LocalAuthMode::RestrictedAgent, None);
        let legacy = json!({"principal":"uid:1000"});
        let error = validate_attach_security(&config, &legacy).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot prove restricted MCP attach")
        );

        let hardened = json!({
            "principal":"agent:mcp",
            "auth_mode":"restricted-agent",
            "session_isolation":"principal",
            "security_epoch": ATTACH_SECURITY_EPOCH,
        });
        validate_attach_security(&config, &hardened).unwrap();
    }

    #[test]
    fn explicit_local_human_and_bearer_keep_deliberate_compatibility() {
        let legacy = json!({"principal":"uid:1000"});
        validate_attach_security(&config(LocalAuthMode::LocalHuman, None), &legacy).unwrap();
        validate_attach_security(
            &config(LocalAuthMode::RestrictedAgent, Some("bearer")),
            &legacy,
        )
        .unwrap();

        let wrong = json!({"auth_mode":"restricted-agent"});
        assert!(
            validate_attach_security(&config(LocalAuthMode::LocalHuman, None), &wrong).is_err()
        );
    }

    #[test]
    fn attach_request_is_explicitly_restricted_without_a_token() {
        let restricted = attach_params(&config(LocalAuthMode::RestrictedAgent, None)).unwrap();
        assert_eq!(restricted["local_auth"], json!("restricted-agent"));
        assert!(restricted["token"].is_null());

        let bearer =
            attach_params(&config(LocalAuthMode::RestrictedAgent, Some("secret"))).unwrap();
        assert!(bearer.get("local_auth").is_none());
        assert_eq!(bearer["token"], json!("secret"));

        assert!(attach_params(&config(LocalAuthMode::LocalHuman, Some("secret"))).is_err());
    }
}
