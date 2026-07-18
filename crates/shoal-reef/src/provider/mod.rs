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

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

/// An admitted provider-discovery result. Providers cannot hand the resolver
/// a raw, potentially unbounded vector: candidates enter this collection one
/// at a time under shared identity and retained-path walls.
#[derive(Debug)]
pub struct CandidateDiscovery {
    provider: &'static str,
    candidates: Vec<Candidate>,
    retained_bytes: usize,
}

pub const MAX_DISCOVERY_CANDIDATES: usize = 4_096;
pub const MAX_DISCOVERY_RETAINED_BYTES: usize = 16 * 1024 * 1024;

impl CandidateDiscovery {
    pub fn new(provider: &'static str) -> Self {
        Self {
            provider,
            candidates: Vec::new(),
            retained_bytes: 0,
        }
    }

    pub fn from_candidates(
        provider: &'static str,
        candidates: impl IntoIterator<Item = Candidate>,
    ) -> Result<Self, ProviderError> {
        let mut discovery = Self::new(provider);
        for candidate in candidates {
            discovery.push(candidate)?;
        }
        Ok(discovery)
    }

    pub fn push(&mut self, candidate: Candidate) -> Result<(), ProviderError> {
        if self.candidates.len() >= MAX_DISCOVERY_CANDIDATES {
            return Err(ProviderError::new(
                self.provider,
                format!("candidate identity limit reached ({MAX_DISCOVERY_CANDIDATES})"),
            ));
        }
        let retained = candidate
            .tool
            .len()
            .checked_add(candidate.version.to_string().len())
            .and_then(|bytes| {
                bytes.checked_add(candidate.path.as_os_str().as_encoded_bytes().len())
            })
            .and_then(|bytes| bytes.checked_add(candidate.provider.len()))
            .and_then(|bytes| bytes.checked_add(128))
            .ok_or_else(|| ProviderError::new(self.provider, "candidate accounting overflowed"))?;
        let next = self.retained_bytes.checked_add(retained).ok_or_else(|| {
            ProviderError::new(self.provider, "candidate aggregate accounting overflowed")
        })?;
        if next > MAX_DISCOVERY_RETAINED_BYTES {
            return Err(ProviderError::new(
                self.provider,
                format!("candidate retained state exceeds {MAX_DISCOVERY_RETAINED_BYTES} bytes"),
            ));
        }
        self.retained_bytes = next;
        self.candidates.push(candidate);
        Ok(())
    }

    pub fn into_candidates(self) -> Vec<Candidate> {
        self.candidates
    }
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

/// One bounded provider-side subprocess request. The runner, rather than the
/// provider, owns environment, sandbox, and cancellation authority.
pub struct ProviderCommand<'a> {
    pub program: &'a Path,
    pub args: &'a [OsString],
    pub cwd: &'a Path,
    pub timeout: Duration,
    pub output_cap: usize,
}

/// Injectable authority for provider probes and installers.
pub trait ProviderRunner: Send + Sync {
    fn run(&self, command: ProviderCommand<'_>) -> io::Result<shoal_exec::BoundedCommandOutput>;
}

#[derive(Debug, Default)]
struct AmbientProviderRunner;

impl ProviderRunner for AmbientProviderRunner {
    fn run(&self, command: ProviderCommand<'_>) -> io::Result<shoal_exec::BoundedCommandOutput> {
        let mut process = std::process::Command::new(command.program);
        process.args(command.args).current_dir(command.cwd);
        shoal_exec::run_bounded_command(&mut process, command.timeout, command.output_cap)
    }
}

/// Context passed to provider operations.
#[derive(Clone)]
pub struct ProviderCtx {
    pub cwd: PathBuf,
    runner: Arc<dyn ProviderRunner>,
    path_env: Option<OsString>,
}

impl std::fmt::Debug for ProviderCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCtx")
            .field("cwd", &self.cwd)
            .field("path_env", &self.path_env)
            .finish_non_exhaustive()
    }
}

impl ProviderCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> ProviderCtx {
        ProviderCtx {
            cwd: cwd.into(),
            runner: Arc::new(AmbientProviderRunner),
            path_env: std::env::var_os("PATH"),
        }
    }

    pub fn with_runner(
        cwd: impl Into<PathBuf>,
        path_env: Option<OsString>,
        runner: Arc<dyn ProviderRunner>,
    ) -> ProviderCtx {
        ProviderCtx {
            cwd: cwd.into(),
            runner,
            path_env,
        }
    }

    pub fn run(
        &self,
        program: &Path,
        args: &[OsString],
        timeout: Duration,
        output_cap: usize,
    ) -> io::Result<shoal_exec::BoundedCommandOutput> {
        self.runner.run(ProviderCommand {
            program,
            args,
            cwd: &self.cwd,
            timeout,
            output_cap,
        })
    }

    /// Resolve a provider helper against this context's exact `PATH`, treating
    /// relative and empty components relative to the provider cwd.
    pub fn which(&self, name: &OsStr) -> Option<PathBuf> {
        shoal_exec::which_in(name, self.path_env.as_deref(), &self.cwd)
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
    fn discover(&self, tool: &str, ctx: &ProviderCtx) -> Result<CandidateDiscovery, ProviderError>;

    /// Resolve the concrete version of a candidate, probing if necessary and
    /// caching the result. Default: return whatever `discover` already knew.
    fn version_of(&self, cand: &Candidate, _ctx: &ProviderCtx) -> Version {
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
pub(crate) fn probe_version(path: &std::path::Path, ctx: &ProviderCtx) -> Version {
    let args = [OsString::from("--version")];
    let output = match ctx.run(path, &args, PROBE_TIMEOUT, PROBE_OUTPUT_CAP) {
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
    use std::os::unix::process::ExitStatusExt;
    use std::sync::Mutex;
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

    #[derive(Default)]
    struct RecordingRunner {
        calls: Mutex<Vec<RecordedCall>>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RecordedCall {
        program: PathBuf,
        args: Vec<OsString>,
        timeout: Duration,
        output_cap: usize,
    }

    impl ProviderRunner for RecordingRunner {
        fn run(
            &self,
            command: ProviderCommand<'_>,
        ) -> io::Result<shoal_exec::BoundedCommandOutput> {
            self.calls.lock().unwrap().push(RecordedCall {
                program: command.program.to_path_buf(),
                args: command.args.to_vec(),
                timeout: command.timeout,
                output_cap: command.output_cap,
            });
            Ok(shoal_exec::BoundedCommandOutput {
                status: std::process::ExitStatus::from_raw(0),
                stdout: b"fixture 7.8.9\n".to_vec(),
                stderr: Vec::new(),
                truncated: false,
                timed_out: false,
                pgid: 1,
                duration: Duration::ZERO,
            })
        }
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
    fn candidate_discovery_rejects_identity_and_retained_byte_overflow() {
        let candidates = (0..=MAX_DISCOVERY_CANDIDATES).map(|index| {
            Candidate::new(
                format!("tool-{index}"),
                Version::unknown(),
                PathBuf::from(format!("/bin/tool-{index}")),
                "fixture",
            )
        });
        let error = CandidateDiscovery::from_candidates("fixture", candidates).unwrap_err();
        assert!(error.msg.contains("identity limit"));

        let mut discovery = CandidateDiscovery::new("fixture");
        let error = discovery
            .push(Candidate::new(
                "x".repeat(MAX_DISCOVERY_RETAINED_BYTES + 1),
                Version::unknown(),
                PathBuf::from("/bin/x"),
                "fixture",
            ))
            .unwrap_err();
        assert!(error.msg.contains("retained state"));
        assert!(discovery.into_candidates().is_empty());
    }

    #[test]
    fn probe_missing_binary_is_unknown() {
        let v = probe_version(
            std::path::Path::new("/nonexistent/tool/xyz"),
            &ProviderCtx::new("/"),
        );
        assert!(v.is_unknown());
    }

    #[test]
    fn probe_uses_injected_runner_with_exact_resource_budget() {
        let runner = Arc::new(RecordingRunner::default());
        let context = ProviderCtx::with_runner("/work", None, runner.clone());
        let version = probe_version(Path::new("/fixture/tool"), &context);
        assert_eq!(version.raw(), "7.8.9");
        assert_eq!(
            *runner.calls.lock().unwrap(),
            vec![RecordedCall {
                program: PathBuf::from("/fixture/tool"),
                args: vec![OsString::from("--version")],
                timeout: PROBE_TIMEOUT,
                output_cap: PROBE_OUTPUT_CAP,
            }]
        );
    }

    #[test]
    fn shipped_provider_hooks_do_not_reacquire_process_authority() {
        for (name, source) in [
            ("mise", include_str!("mise.rs")),
            ("system", include_str!("system.rs")),
        ] {
            for forbidden in [
                "std::process::Command",
                "Command::new(",
                ".status()",
                ".output()",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "{name} provider bypasses ProviderRunner with `{forbidden}`"
                );
            }
        }
    }

    #[test]
    fn probe_drains_flooding_output_without_defeating_deadline() {
        let (_dir, tool) = executable_script(
            "printf 'hostile-tool 1.2.3\\n'; i=0; while [ $i -lt 50000 ]; do printf 0123456789; i=$((i+1)); done",
        );
        let start = Instant::now();
        let version = probe_version(&tool, &ProviderCtx::new("/"));
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
        assert!(probe_version(&tool, &ProviderCtx::new("/")).is_unknown());
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
