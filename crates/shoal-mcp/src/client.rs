//! Kernel connection: `Config`, Unix-socket discovery, and the JSON-RPC
//! `KernelClient` used to talk to `shoal-kernel` over its Unix socket.

use crate::{read_json_line, write_json_line};
use serde_json::{Value, json};
use shoal_proto::{ATTACH_SECURITY_EPOCH, LocalAuthMode, PRINCIPAL_SESSION_ISOLATION};
use std::io::{self, BufReader};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

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
        let stream = UnixStream::connect(&config.socket)?;
        Self::from_stream(stream, config, "mcp", false, LocalAuthMode::RestrictedAgent)
    }

    /// Attach as a human over the inherited anonymous transport created by the
    /// private REPL. Named/public socket clients must use [`Self::connect`],
    /// which has no API for asserting human presence.
    pub fn from_embedded_human_stream(
        stream: UnixStream,
        config: &Config,
        client_kind: &str,
        tty: bool,
    ) -> Result<Self, BridgeError> {
        Self::from_stream(stream, config, client_kind, tty, LocalAuthMode::LocalHuman)
    }

    fn from_stream(
        stream: UnixStream,
        config: &Config,
        client_kind: &str,
        tty: bool,
        local_auth: LocalAuthMode,
    ) -> Result<Self, BridgeError> {
        let params = attach_params_for(config, client_kind, tty, local_auth)?;
        let mut client = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_id: 1,
            attach: Value::Null,
        };
        client.attach = client.call("session.attach", params)?;
        validate_attach_security(config, local_auth, &client.attach)?;
        Ok(client)
    }

    pub(crate) fn shutdown_handle(&self) -> io::Result<UnixStream> {
        self.writer.try_clone()
    }

    pub(crate) fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.reader.get_ref().set_read_timeout(timeout)
    }

    pub(crate) fn read_frame(&mut self) -> Result<Option<Value>, BridgeError> {
        read_json_line(&mut self.reader)
    }

    pub(crate) fn read_fd(&self) -> RawFd {
        self.reader.get_ref().as_raw_fd()
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, BridgeError> {
        self.call_with_notifications(method, params, |_| {})
    }

    /// Make one request while preserving interleaved push notifications.
    /// The multiplexed subscription owner uses this so adding/removing one
    /// channel cannot drop an event already queued for another channel.
    pub(crate) fn call_with_notifications(
        &mut self,
        method: &str,
        params: Value,
        mut notification: impl FnMut(&Value),
    ) -> Result<Value, BridgeError> {
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
                notification(&frame);
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

#[cfg(test)]
fn attach_params(config: &Config) -> Result<Value, BridgeError> {
    attach_params_for(config, "mcp", false, LocalAuthMode::RestrictedAgent)
}

fn attach_params_for(
    config: &Config,
    client_kind: &str,
    tty: bool,
    local_auth: LocalAuthMode,
) -> Result<Value, BridgeError> {
    if config.token.is_some() && local_auth == LocalAuthMode::LocalHuman {
        return Err(BridgeError::Protocol(
            "bearer authentication and embedded local-human authentication are mutually exclusive"
                .into(),
        ));
    }
    let mut params = json!({
        "session": config.session,
        "token": config.token,
        "client": {"kind":client_kind, "tty":tty}
    });
    if config.token.is_none() {
        params["local_auth"] = serde_json::to_value(local_auth)?;
    }
    Ok(params)
}

/// Refuse silent security downgrades from kernels that cannot prove the
/// requested authority and principal-isolation boundary. Even the private REPL
/// path requires hardened metadata; a legacy response is never accepted as
/// evidence of human presence.
fn validate_attach_security(
    config: &Config,
    requested_auth: LocalAuthMode,
    attach: &Value,
) -> Result<(), BridgeError> {
    if config.token.is_some() {
        return Ok(());
    }
    match requested_auth {
        LocalAuthMode::LocalHuman => {
            let mode = attach.get("auth_mode").and_then(Value::as_str);
            let isolation = attach.get("session_isolation").and_then(Value::as_str);
            let epoch = attach.get("security_epoch").and_then(Value::as_u64);
            let principal = attach.get("principal").and_then(Value::as_str);
            if mode != Some("local-human")
                || isolation != Some(PRINCIPAL_SESSION_ISOLATION)
                || epoch.is_none_or(|v| v < u64::from(ATTACH_SECURITY_EPOCH))
                || !principal.is_some_and(|p| p.starts_with("uid:"))
            {
                return Err(BridgeError::Protocol(
                    "kernel cannot prove a private local-human attach and principal-isolated \
                     sessions; upgrade shoal-kernel"
                        .into(),
                ));
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
                     upgrade shoal-kernel or provide a bearer token"
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

    fn config(token: Option<&str>) -> Config {
        Config {
            socket: PathBuf::from("/tmp/not-used.sock"),
            session: Some("test".into()),
            token: token.map(str::to_owned),
        }
    }

    #[test]
    fn restricted_attach_requires_hardened_kernel_metadata() {
        let config = config(None);
        let legacy = json!({"principal":"uid:1000"});
        let error =
            validate_attach_security(&config, LocalAuthMode::RestrictedAgent, &legacy).unwrap_err();
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
        validate_attach_security(&config, LocalAuthMode::RestrictedAgent, &hardened).unwrap();
    }

    #[test]
    fn private_local_human_requires_hardened_metadata() {
        let legacy = json!({"principal":"uid:1000"});
        assert!(
            validate_attach_security(&config(None), LocalAuthMode::LocalHuman, &legacy).is_err()
        );
        let hardened = json!({
            "principal":"uid:1000",
            "auth_mode":"local-human",
            "session_isolation":"principal",
            "security_epoch": ATTACH_SECURITY_EPOCH,
        });
        validate_attach_security(&config(None), LocalAuthMode::LocalHuman, &hardened).unwrap();

        validate_attach_security(
            &config(Some("bearer")),
            LocalAuthMode::RestrictedAgent,
            &legacy,
        )
        .unwrap();
    }

    #[test]
    fn attach_request_is_explicitly_restricted_without_a_token() {
        let restricted = attach_params(&config(None)).unwrap();
        assert_eq!(restricted["local_auth"], json!("restricted-agent"));
        assert!(restricted["token"].is_null());

        let bearer = attach_params(&config(Some("secret"))).unwrap();
        assert!(bearer.get("local_auth").is_none());
        assert_eq!(bearer["token"], json!("secret"));

        assert!(
            attach_params_for(
                &config(Some("secret")),
                "shoal-repl",
                true,
                LocalAuthMode::LocalHuman,
            )
            .is_err()
        );
    }
}
