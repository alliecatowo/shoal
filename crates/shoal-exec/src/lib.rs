//! Process execution engine for the shoal shell.
//!
//! Blocking and thread-based (no async runtime). Two execution modes, per
//! TDD §1.2 (the PTY position rule — this crate implements the mechanism):
//!
//! - [`ExecMode::Capture`] — value position: stdout/stderr are pipes, stdin is
//!   configured per [`StdinSpec`], the child has no controlling tty and is
//!   placed in its own process group (`setpgid(0, 0)`). Both pipes are drained
//!   concurrently so a child filling either one can never deadlock.
//! - [`ExecMode::PtyTee`] — statement position: the child runs on a real PTY
//!   as session leader. Output streams raw to the real terminal *and* is teed
//!   into the returned buffer; real stdin is forwarded to the PTY with the
//!   terminal in raw mode for the duration (restored panic-safely by a drop
//!   guard); window-size changes are propagated by polling. When the hosting
//!   process has no tty (tests, CI), the child still gets a PTY but raw mode
//!   and stdin forwarding are skipped gracefully.
//!
//! Cancellation (TDD §4.7): a [`CancelToken`] is polled roughly every 50 ms;
//! once cancelled, the child's *process group* receives `SIGINT`, escalating
//! to `SIGTERM` after 3 s and `SIGKILL` after 3 more. The run then returns
//! normally with the fatal signal recorded by name.
//!
//! Signal deaths (TDD §13.6) surface as `signal: Some("SIGSEGV")` etc. with
//! `status: None` — never the shell-style `128+n` encoding. Children are
//! always reaped (no zombies), and spawn failures such as `E2BIG` surface as
//! [`std::io::Error`].

mod cancel;
mod capture;
mod pty;
mod sandbox;
mod status;
mod watcher;
mod which;

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

pub use cancel::CancelToken;
pub use capture::{StreamingChild, spawn_capture};
pub use which::which;

/// A fully-resolved request to execute one external process.
#[derive(Debug, Clone)]
pub struct ExecSpec {
    /// `argv[0]` is the program. If it contains a `/` it is executed as-is;
    /// otherwise it is resolved with [`which`] against the `PATH` entry of
    /// [`ExecSpec::env`] (falling back to the host process `PATH` when the
    /// spec's environment carries none).
    pub argv: Vec<OsString>,
    /// Working directory for the child.
    pub cwd: PathBuf,
    /// The **complete** child environment — nothing is inherited implicitly.
    pub env: Vec<(OsString, OsString)>,
    /// What the child sees on stdin (see [`StdinSpec`] for per-mode notes).
    pub stdin: StdinSpec,
    /// Capture (value position) or PTY-tee (statement position).
    pub mode: ExecMode,
    /// Optional OS-enforcement request (TDD §8). `None` (the default) is the
    /// existing unsandboxed behavior. When `Some`, [`run`]/[`spawn_capture`]
    /// apply the strongest available mechanism before exec in the child and
    /// report what actually happened via [`ExecResult::enforcement`]; see
    /// [`shoal_leash::SandboxPolicy`].
    pub sandbox: Option<shoal_leash::SandboxPolicy>,
}

/// What to connect to the child's stdin.
///
/// In [`ExecMode::Capture`] the variants map directly onto the child's fd 0.
/// In [`ExecMode::PtyTee`] the child's stdin is always the PTY slave;
/// `Inherit` forwards the real terminal's input (when it is a tty), while
/// `Bytes`/`File` write the given data into the PTY master and `Null` sends
/// nothing.
#[derive(Debug, Clone)]
pub enum StdinSpec {
    /// `/dev/null` (Capture) / nothing forwarded (PtyTee).
    Null,
    /// Inherit the parent's stdin (Capture) / forward the real tty (PtyTee).
    Inherit,
    /// Feed the given bytes, then close (Capture) or stop writing (PtyTee).
    Bytes(Vec<u8>),
    /// Feed the contents of a file.
    File(PathBuf),
}

/// Execution mode — the mechanism behind the TDD §1.2 PTY position rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    /// stdout/stderr = pipes, stdin per spec, no controlling tty, child in
    /// its own process group.
    Capture,
    /// Child on a real PTY (its own session): bytes stream raw to the REAL
    /// terminal AND are teed into the returned stdout buffer; real stdin is
    /// forwarded to the PTY (terminal in raw mode for the duration); window
    /// resizes propagated; stderr merged into stdout (pty semantics).
    PtyTee,
}

/// The outcome of one completed child process.
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// `Some(code)` on normal exit; `None` when the child died to a signal.
    pub status: Option<i32>,
    /// `Some("SIGSEGV")` etc. on signal death (never a `128+n` encoding).
    pub signal: Option<String>,
    /// Captured bytes (PtyTee: the teed, merged stream).
    pub stdout: Vec<u8>,
    /// Captured bytes (PtyTee: empty — stderr is merged into the pty stream).
    pub stderr: Vec<u8>,
    /// Wall-clock time from spawn to reap.
    pub dur: std::time::Duration,
    /// The child's process id (also its process-group id).
    pub pid: u32,
    /// `Some` iff `ExecSpec::sandbox` was set, reporting the OS-enforcement
    /// tier that was **actually** applied to this child (TDD §8 tier
    /// honesty) — never `enforced: true` unless it really was. `None` means
    /// no sandbox was requested; it does not mean one was silently applied.
    pub enforcement: Option<shoal_leash::EnforcementStatus>,
}

/// Run `spec` to completion, blocking the calling thread.
///
/// Cancellation: once `cancel` trips, the child's process group receives
/// `SIGINT`, escalating to `SIGTERM` after 3 s and `SIGKILL` after 3 more.
/// The function still returns `Ok` in that case, with [`ExecResult::signal`]
/// recording how the child died.
///
/// # Errors
///
/// Returns an [`io::Error`] when the program cannot be resolved
/// ([`io::ErrorKind::NotFound`]), when spawning fails (`E2BIG` surfaces here
/// with its OS error code), or when PTY/pipe plumbing fails.
pub fn run(spec: ExecSpec, cancel: &CancelToken) -> io::Result<ExecResult> {
    match spec.mode {
        ExecMode::Capture => capture::run_capture(spec, cancel),
        ExecMode::PtyTee => pty::run_pty(spec, cancel),
    }
}

/// Run through the child-only Landlock/Seatbelt launcher, always with a
/// **hard** guarantee: the parent is never restricted, and the request
/// fails closed (no spawn at all) if the strongest backend on this platform
/// is unavailable, rather than ever running unconfined. Prefer
/// `ExecSpec::sandbox` (with `hermetic: true` for the same fail-closed
/// guarantee, or `false` to degrade honestly instead of refusing) for new
/// callers — this function is kept for source compatibility.
pub fn run_sandboxed(
    spec: ExecSpec,
    cancel: &CancelToken,
    sandbox: shoal_leash::FsSandbox,
    verified: Option<&shoal_leash::SpawnPreflight>,
) -> io::Result<ExecResult> {
    let hard_won = verified.is_some();
    let wrapped = sandbox_spec(spec, sandbox, verified)?;
    let mut result = run(wrapped, cancel)?;
    result
        .enforcement
        .get_or_insert_with(|| hard_landlock_status(hard_won));
    Ok(result)
}

/// Streaming capture variant of [`run_sandboxed`].
pub fn spawn_capture_sandboxed(
    spec: ExecSpec,
    cancel: &CancelToken,
    sandbox: shoal_leash::FsSandbox,
    verified: Option<&shoal_leash::SpawnPreflight>,
) -> io::Result<StreamingChild> {
    if spec.mode != ExecMode::Capture {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "spawn_capture_sandboxed requires Capture mode",
        ));
    }
    spawn_capture(sandbox_spec(spec, sandbox, verified)?, cancel)
}

fn hard_landlock_status(spawn_exec_enforced: bool) -> shoal_leash::EnforcementStatus {
    shoal_leash::EnforcementStatus {
        available_tier: shoal_leash::EnforcementTier::A,
        active_tier: Some(shoal_leash::EnforcementTier::A),
        enforced: true,
        detail: "Landlock applied via run_sandboxed's hard-requirement helper wrapping".into(),
        landlock_abi: shoal_leash::landlock_abi(),
        filesystem_enforced: true,
        spawn_exec_enforced,
        network_enforced: false,
    }
}

fn sandbox_spec(
    mut spec: ExecSpec,
    sandbox: shoal_leash::FsSandbox,
    verified: Option<&shoal_leash::SpawnPreflight>,
) -> io::Result<ExecSpec> {
    if shoal_leash::landlock_abi().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hard Landlock enforcement unavailable",
        ));
    }
    let program = which::resolve_program(&spec.argv, &spec.env)?;
    if let Some(expected) = verified {
        let actual = shoal_leash::preflight_spawn(&program, std::slice::from_ref(&expected.hash))?;
        if !expected.allowed || actual.hash != expected.hash {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "verified spawn hash does not match resolved binary",
            ));
        }
    }
    let helper = sandbox::sandbox_helper()?;
    spec.argv = sandbox::wrap(helper, &sandbox, program, &spec.argv);
    Ok(spec)
}
