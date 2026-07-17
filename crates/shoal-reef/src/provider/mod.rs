//! Providers — acquisition as adapters (site/content/internals/reef-resolution.md).
//!
//! A provider *enumerates* candidates for a tool (fast, cached, never probing at
//! enumerate time) and optionally *fetches* (installs) one. Only `fetch` may
//! touch the network, and only the mise provider implements it in v1.
//!
//! The trait deviates from site/content/internals/reef-resolution.md's sketch in one deliberate way: `discover` and
//! `fetch` take a [`ProviderCtx`] carrying the cwd, because `npm-local` and
//! `venv` — both listed in site/content/internals/reef-resolution.md — are inherently cwd-relative. Version
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
        Candidate {
            tool: tool.into(),
            version,
            path,
            provider,
            ambient: false,
        }
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
        ProviderError {
            provider,
            msg: msg.into(),
        }
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

/// The `--version` probe timeout (site/content/internals/reef-resolution.md).
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// Version probes only need a short banner. Retaining more would let a hostile
/// tool amplify memory use during ordinary resolution; the executor continues
/// draining both pipes after this cap so the child cannot block on them.
const PROBE_OUTPUT_CAP: usize = 16 * 1024;

/// Run `<path> --version` with a hard timeout and parse a version leniently from
/// its output. Returns [`Version::unknown`] on timeout, spawn failure, or when no
/// version-shaped token is found. Never blocks longer than [`PROBE_TIMEOUT`].
pub(crate) fn probe_version(path: &std::path::Path) -> Version {
    let mut command = std::process::Command::new(path);
    command.arg("--version");
    let output =
        match shoal_exec::run_bounded_command(&mut command, PROBE_TIMEOUT, PROBE_OUTPUT_CAP) {
            Ok(output) if !output.timed_out => output,
            Err(_) => return Version::unknown(),
            Ok(_) => return Version::unknown(),
        };

    let mut out = String::from_utf8_lossy(&output.stdout).into_owned();
    out.push(' ');
    out.push_str(&String::from_utf8_lossy(&output.stderr));
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
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::thread;
    use std::time::Instant;

    fn executable_script(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tool");
        fs::write(&path, format!("#!/bin/sh\n{contents}\n")).expect("write tool");
        let mut permissions = fs::metadata(&path).expect("tool metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("make tool executable");
        (dir, path)
    }

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

    #[test]
    fn probe_drains_flooding_output_without_defeating_deadline() {
        let (_dir, tool) = executable_script(
            "printf 'hostile-tool 1.2.3\\n'; i=0; while [ $i -lt 50000 ]; do printf 0123456789; i=$((i+1)); done",
        );
        let start = Instant::now();
        let version = probe_version(&tool);
        assert_eq!(version.raw(), "1.2.3");
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn probe_timeout_kills_forked_descendants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let descendant_path = dir.path().join("descendant.pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", descendant_path.display());
        let (_tool_dir, tool) = executable_script(&script);

        let start = Instant::now();
        assert!(probe_version(&tool).is_unknown());
        assert!(start.elapsed() < Duration::from_secs(1));

        let descendant: libc::pid_t = fs::read_to_string(descendant_path)
            .expect("descendant pid")
            .trim()
            .parse()
            .expect("numeric pid");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY: signal 0 only checks whether this recorded pid exists.
            let result = unsafe { libc::kill(descendant, 0) };
            if result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "descendant {descendant} survived"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
