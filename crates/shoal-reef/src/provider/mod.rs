//! Providers — acquisition as adapters (REEF.md §3).
//!
//! A provider *enumerates* candidates for a tool (fast, cached, never probing at
//! enumerate time) and optionally *fetches* (installs) one. Only `fetch` may
//! touch the network, and only the mise provider implements it in v1.
//!
//! The trait deviates from REEF's sketch in one deliberate way: `discover` and
//! `fetch` take a [`ProviderCtx`] carrying the cwd, because `npm-local` and
//! `venv` — both listed in REEF §3 — are inherently cwd-relative. Version
//! probing is factored into [`Provider::version_of`] so the resolver can probe
//! lazily, only when a constraint actually needs a concrete version.

use std::path::PathBuf;
use std::time::Duration;

use crate::version::{Constraint, Version};

mod cargo;
mod mise;
mod npm;
mod system;
mod venv;

pub use cargo::CargoProvider;
pub use mise::MiseProvider;
pub use npm::NpmLocalProvider;
pub use system::SystemProvider;
pub use venv::VenvProvider;

/// A discovered tool candidate.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub tool: String,
    pub version: Version,
    pub path: PathBuf,
    pub provider: &'static str,
    /// `true` when this came from an ambient `$PATH` dir (system provider only),
    /// reported as scope `ambient` in diagnostics.
    pub ambient: bool,
}

impl Candidate {
    pub fn new(
        tool: impl Into<String>,
        version: Version,
        path: PathBuf,
        provider: &'static str,
    ) -> Candidate {
        Candidate { tool: tool.into(), version, path, provider, ambient: false }
    }
}

/// Context passed to provider operations.
#[derive(Debug, Clone)]
pub struct ProviderCtx {
    pub cwd: PathBuf,
}

impl ProviderCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> ProviderCtx {
        ProviderCtx { cwd: cwd.into() }
    }
}

/// A provider-side failure (probe error, malformed layout, install failure).
#[derive(Debug, Clone)]
pub struct ProviderError {
    pub provider: &'static str,
    pub msg: String,
}

impl ProviderError {
    pub fn new(provider: &'static str, msg: impl Into<String>) -> ProviderError {
        ProviderError { provider, msg: msg.into() }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.provider, self.msg)
    }
}
impl std::error::Error for ProviderError {}

/// A tool-acquisition backend.
pub trait Provider: Send + Sync {
    /// Stable provider name (`"system"`, `"mise"`, …).
    fn name(&self) -> &'static str;

    /// Enumerate candidates for `tool`. Must be fast and must **not** probe
    /// `--version` — leave [`Candidate::version`] as [`Version::unknown`] when
    /// the version is not free to compute (e.g. from a directory name).
    fn discover(&self, tool: &str, ctx: &ProviderCtx) -> Vec<Candidate>;

    /// Resolve the concrete version of a candidate, probing if necessary and
    /// caching the result. Default: return whatever `discover` already knew.
    fn version_of(&self, cand: &Candidate) -> Version {
        cand.version.clone()
    }

    /// Optionally materialize a version satisfying `req` (may download). Returns
    /// `None` when this provider cannot install. Default: `None`.
    fn fetch(
        &self,
        tool: &str,
        req: &Constraint,
        ctx: &ProviderCtx,
    ) -> Option<Result<Candidate, ProviderError>> {
        let _ = (tool, req, ctx);
        None
    }
}

/// The `--version` probe timeout (REEF.md §3).
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// Run `<path> --version` with a hard timeout and parse a version leniently from
/// its output. Returns [`Version::unknown`] on timeout, spawn failure, or when no
/// version-shaped token is found. Never blocks longer than [`PROBE_TIMEOUT`].
pub(crate) fn probe_version(path: &std::path::Path) -> Version {
    use std::process::{Command, Stdio};

    let mut child = match Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Version::unknown(),
    };

    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Version::unknown();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return Version::unknown(),
        }
    }

    let mut out = String::new();
    if let Ok(o) = child.wait_with_output() {
        out.push_str(&String::from_utf8_lossy(&o.stdout));
        out.push(' ');
        out.push_str(&String::from_utf8_lossy(&o.stderr));
    }
    parse_version_token(&out)
}

/// Extract the first version-shaped token (a dotted integer run, optional `v`
/// prefix) from arbitrary `--version` output.
pub(crate) fn parse_version_token(s: &str) -> Version {
    for raw in s.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',') {
        let tok = raw.trim().trim_start_matches('v');
        let mut chars = tok.chars();
        // Must start with a digit and contain at least one '.'.
        if chars.next().map(|c| c.is_ascii_digit()) == Some(true) && tok.contains('.') {
            let v = Version::parse(tok);
            if !v.is_opaque() {
                return v;
            }
        }
    }
    // No dotted token: try a bare leading integer (e.g. "22").
    for raw in s.split_whitespace() {
        let tok = raw.trim_start_matches('v');
        if !tok.is_empty() && tok.chars().all(|c| c.is_ascii_digit()) {
            return Version::parse(tok);
        }
    }
    Version::unknown()
}

/// Is `path` a regular file (or symlink to one) with an executable bit set?
pub(crate) fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_token_variants() {
        assert_eq!(parse_version_token("git version 2.43.0").raw(), "2.43.0");
        assert_eq!(parse_version_token("Python 3.12.4").raw(), "3.12.4");
        assert_eq!(parse_version_token("v22.3.0\n").raw(), "22.3.0");
        assert_eq!(parse_version_token("node (v20.11.1)").raw(), "20.11.1");
        assert!(parse_version_token("no version here").is_unknown());
    }

    #[test]
    fn probe_missing_binary_is_unknown() {
        let v = probe_version(std::path::Path::new("/nonexistent/tool/xyz"));
        assert!(v.is_unknown());
    }
}
