//! Process execution engine for the shoal shell.
//!
//! Blocking and thread-based (no async runtime). Two execution modes, per
//! site/content/internals/language-conformance-contract.md (the PTY position rule — this crate implements the mechanism):
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
//! Cancellation (site/content/internals/language-conformance-contract.md): a [`CancelToken`] is polled roughly every 50 ms;
//! once cancelled, the child's *process group* receives `SIGINT`, escalating
//! to `SIGTERM` after 3 s and `SIGKILL` after 3 more. The run then returns
//! normally with the fatal signal recorded by name.
//!
//! Signal deaths (site/content/internals/language-conformance-contract.md) surface as `signal: Some("SIGSEGV")` etc. with
//! `status: None` — never the shell-style `128+n` encoding. Children are
//! always reaped (no zombies), and spawn failures such as `E2BIG` surface as
//! [`std::io::Error`].
//!
//! Job control (site/content/internals/language-conformance-contract.md): a PtyTee **foreground** child is waited on with
//! `WUNTRACED`, so a Ctrl-Z (SIGTSTP delivered to the child's foreground
//! process group by the pty line discipline) is observable as a *stop* rather
//! than hanging the shell. On a stop, [`run`] returns an [`ExecResult`] with
//! `stopped: true` and parks the still-live PTY (master + child) so the host
//! can later resume it in the foreground (`fg`) or background (`bg`) via
//! [`PtyJob`]/[`take_stopped_job`]. This is strictly additive: a child that
//! runs to completion behaves byte-identically to before, and Capture mode has
//! no stop concept at all.

mod cancel;
mod capture;
mod pty;
mod pty_session;
mod sandbox;
mod status;
mod watcher;
mod which;

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub use cancel::CancelToken;
pub use capture::{StreamingChild, spawn_capture};
pub use pty::{PtyJob, shutdown_stopped_jobs, take_stopped_job};
pub use pty_session::{
    PTY_DEFAULT_COLS, PTY_DEFAULT_ROWS, PTY_MAX_COLS, PTY_MAX_ROWS, PtyOpenSpec, PtySession,
    ScreenSnapshot, named_key,
};
pub use which::which;

/// Resolve `argv[0]` (an absolute path as-is, or a bare name via the `PATH`
/// entry of `env`) and return the blake3-hex of its on-disk bytes — the same
/// digest `shoal_reef::hashcache::hash_bytes` / `shoal_leash::preflight_spawn`
/// produce, so a `proc_spawn` pin an author copies from `reef`/`which` compares
/// equal. `None` when the binary can't be located or read. Used by kernel hosts
/// to build the [`shoal_leash::Effect::ProcSpawn`] `bin_hash` for the PTY-open
/// spawn gate without re-implementing resolution/hashing.
#[must_use]
pub fn resolve_and_hash(argv: &[OsString], env: &[(OsString, OsString)]) -> Option<String> {
    let program = which::resolve_program(argv, env).ok()?;
    let bytes = std::fs::read(&program).ok()?;
    Some(blake3::hash(&bytes).to_hex().to_string())
}

/// Default hard cap on the bytes buffered in memory when capturing a command's
/// output in value position (site/content/internals/language-conformance-contract.md). Once a captured buffer reaches this
/// size it stops growing in RAM. 64 MiB is high enough that ordinary command
/// output is never affected, yet low enough that `let x = (yes)` /
/// `let x = (cat /dev/zero)` cannot OOM the shell.
///
/// What happens past the cap depends on whether the spec requested a spill
/// ([`ExecSpec::spill`]): with no spill, overflow is discarded and
/// [`ExecResult::truncated`] is set (the RAM floor, unchanged); with a spill,
/// the full stream is streamed to a disk file (blake3-addressed, up to
/// [`capture_spill_cap`]) and returned as [`ExecResult::stdout_spill`] so the
/// caller can adopt it into the CAS as a ref-backed value (site/content/internals/language-conformance-contract.md
/// disk-spill promise). The 64 MiB resident buffer is then the value's preview.
pub const DEFAULT_CAPTURE_HARD_CAP: usize = 64 * 1024 * 1024;

/// Default hard cap on the bytes streamed to a **disk** spill file when a
/// capture requests one ([`ExecSpec::spill`]) and its output exceeds the RAM
/// cap. Bounds disk the way [`DEFAULT_CAPTURE_HARD_CAP`] bounds RAM: without
/// it, `let x = (yes)` would fill the disk instead of OOMing. 1 GiB is large
/// enough that real captures (build logs, `cat huge.log`) spill whole; past it
/// the spill stops and [`CaptureSpill::truncated`] is set.
pub const DEFAULT_CAPTURE_SPILL_CAP: u64 = 1024 * 1024 * 1024;

/// `0` sentinel = not yet resolved; the first [`capture_hard_cap`] call seeds it
/// from the `SHOAL_CAPTURE_CAP_BYTES` env var (a positive integer) or the
/// default, so the cap is configurable without an API/ABI change to `ExecSpec`.
static CAPTURE_HARD_CAP: AtomicUsize = AtomicUsize::new(0);

/// The active in-memory capture hard cap in bytes (see
/// [`DEFAULT_CAPTURE_HARD_CAP`]). Resolved once from `SHOAL_CAPTURE_CAP_BYTES`
/// or the default, unless overridden by [`set_capture_hard_cap`].
pub fn capture_hard_cap() -> usize {
    let cached = CAPTURE_HARD_CAP.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let resolved = std::env::var("SHOAL_CAPTURE_CAP_BYTES")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_CAPTURE_HARD_CAP);
    CAPTURE_HARD_CAP.store(resolved, Ordering::Relaxed);
    resolved
}

/// Override the in-memory capture hard cap (bytes). For hosts wiring config and
/// for tests; `0` is clamped to `1` so the cap is always positive.
pub fn set_capture_hard_cap(bytes: usize) {
    CAPTURE_HARD_CAP.store(bytes.max(1), Ordering::Relaxed);
}

/// `0` sentinel = not yet resolved (see [`CAPTURE_HARD_CAP`]).
static CAPTURE_SPILL_CAP: AtomicU64 = AtomicU64::new(0);

/// The active disk-spill hard cap in bytes (see [`DEFAULT_CAPTURE_SPILL_CAP`]).
/// Resolved once from `SHOAL_CAPTURE_SPILL_CAP_BYTES` or the default, unless
/// overridden by [`set_capture_spill_cap`].
pub fn capture_spill_cap() -> u64 {
    let cached = CAPTURE_SPILL_CAP.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let resolved = std::env::var("SHOAL_CAPTURE_SPILL_CAP_BYTES")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_CAPTURE_SPILL_CAP);
    CAPTURE_SPILL_CAP.store(resolved, Ordering::Relaxed);
    resolved
}

/// Override the disk-spill hard cap (bytes). For hosts wiring config and for
/// tests; `0` is clamped to `1` so the cap is always positive.
pub fn set_capture_spill_cap(bytes: u64) {
    CAPTURE_SPILL_CAP.store(bytes.max(1), Ordering::Relaxed);
}

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
    /// Optional OS-enforcement request (site/content/internals/language-conformance-contract.md). `None` (the default) is the
    /// existing unsandboxed behavior. When `Some`, [`run`]/[`spawn_capture`]
    /// apply the strongest available mechanism before exec in the child and
    /// report what actually happened via [`ExecResult::enforcement`]; see
    /// [`shoal_leash::SandboxPolicy`].
    pub sandbox: Option<shoal_leash::SandboxPolicy>,
    /// Optional disk-spill request for value-position capture (site/content/internals/language-conformance-contract.md).
    /// `None` (the default) preserves the pre-spill behavior *exactly*: stdout
    /// is buffered up to [`capture_hard_cap`] and any overflow is discarded
    /// with [`ExecResult::truncated`] set. When `Some`, a capture whose stdout
    /// exceeds the RAM cap streams the **full** stream to a blake3-addressed
    /// file under [`SpillConfig::dir`] (bounded by [`capture_spill_cap`]),
    /// returned as [`ExecResult::stdout_spill`]; the resident buffer becomes a
    /// bounded preview. Only honored in [`ExecMode::Capture`].
    pub spill: Option<SpillConfig>,
}

/// Where and whether a value-position capture may spill oversized stdout to
/// disk (site/content/internals/language-conformance-contract.md). The caller (which owns the content-addressed store) hands
/// in a directory it will later adopt the spill file from; keeping the CAS
/// itself out of `shoal-exec` preserves the crate's dependency layering.
#[derive(Debug, Clone)]
pub struct SpillConfig {
    /// Directory the spill file is created in. Should be on the same
    /// filesystem the caller ingests it into. Must already exist.
    pub dir: PathBuf,
}

/// A value-position capture whose stdout exceeded [`capture_hard_cap`] and was
/// streamed to disk (site/content/internals/language-conformance-contract.md). The file at [`CaptureSpill::path`] holds the
/// captured bytes (the full stream unless [`CaptureSpill::truncated`]), and
/// [`CaptureSpill::hash`] is their blake3, so the caller can adopt it into a
/// content-addressed store as a ref-backed value. The caller **owns** the file:
/// it must move it into the store or delete it.
#[derive(Debug, Clone)]
pub struct CaptureSpill {
    /// Path of the on-disk spill file (caller-owned; see the type docs).
    pub path: PathBuf,
    /// blake3 hex of the bytes actually stored in the file.
    pub hash: String,
    /// Length in bytes of the stored content (== the value's true length).
    pub len: u64,
    /// `true` when the stream itself exceeded [`capture_spill_cap`] and the
    /// spill was truncated to that bound (the stored bytes are a prefix).
    pub truncated: bool,
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

/// Execution mode — the mechanism behind the site/content/internals/language-conformance-contract.md PTY position rule.
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
    /// `true` when captured output hit the in-memory hard cap
    /// ([`capture_hard_cap`]) and was truncated: the buffered `stdout`/`stderr`
    /// (or the PtyTee `stdout`) is a prefix of what the child actually produced.
    /// The child still ran to completion and, for PtyTee, the real terminal saw
    /// the full stream — only the returned buffer is bounded.
    ///
    /// When [`ExecResult::stdout_spill`] is `Some`, stdout was **not** lost to
    /// this flag — it overflowed the RAM cap but was preserved on disk; `stdout`
    /// here is the bounded preview. `truncated` then reflects only stderr (or a
    /// spill that itself hit [`capture_spill_cap`], mirrored in
    /// [`CaptureSpill::truncated`]).
    pub truncated: bool,
    /// `Some` when this was a value-position capture ([`ExecSpec::spill`] set)
    /// whose stdout overflowed the RAM cap and was streamed to disk (site/content/internals/language-conformance-contract.md).
    /// The caller owns the referenced file and must adopt it into its CAS (or
    /// delete it). `None` on every other path — no spill was requested, or
    /// stdout fit within the RAM cap and is fully resident in `stdout`.
    pub stdout_spill: Option<CaptureSpill>,
    /// Wall-clock time from spawn to reap.
    pub dur: std::time::Duration,
    /// The child's process id (also its process-group id).
    pub pid: u32,
    /// The child's process-group id. Every child is placed in its own process
    /// group (Capture: `setpgid(0, 0)` in the child; PtyTee: the child is a
    /// session leader via `setsid`, so its group id equals its pid). Job control
    /// (site/content/internals/language-conformance-contract.md) signals the whole group via `kill(-pgid, …)`.
    pub pgid: u32,
    /// `true` when a PtyTee foreground child was **stopped** (SIGTSTP/SIGSTOP —
    /// e.g. the user pressed Ctrl-Z) rather than having exited. The child is
    /// alive and suspended, its live PTY parked for resumption (see
    /// [`take_stopped_job`]); `status`/`signal` are both `None`. Always `false`
    /// for Capture and for any normally-terminated child. A [`run`] caller that
    /// never uses job control can ignore this field — it stays `false` unless a
    /// real terminal stop occurs.
    pub stopped: bool,
    /// `Some` iff `ExecSpec::sandbox` was set, reporting the OS-enforcement
    /// tier that was **actually** applied to this child (site/content/internals/language-conformance-contract.md tier
    /// honesty) — never `enforced: true` unless it really was. `None` means
    /// no sandbox was requested; it does not mean one was silently applied.
    pub enforcement: Option<shoal_leash::EnforcementStatus>,
}

/// Install the interactive shell's job-control signal dispositions (site/content/internals/language-conformance-contract.md):
/// a no-op **handler** (not `SIG_IGN`) for `SIGTSTP`, `SIGTTOU`, and `SIGTTIN`,
/// so the shell itself is never suspended by a stray Ctrl-Z or by the terminal-
/// control operations of the handoff. Crucially this uses a handler rather than
/// `SIG_IGN` because `exec` resets *caught* signals to `SIG_DFL` in a child
/// while `SIG_IGN` would persist across exec — so spawned children still get the
/// default disposition, which is exactly what lets Ctrl-Z stop them on their pty.
/// Idempotent; a host (the REPL) calls this once at startup when interactive.
pub fn install_shell_job_control_signals() {
    extern "C" fn noop(_sig: libc::c_int) {}
    for sig in [libc::SIGTSTP, libc::SIGTTOU, libc::SIGTTIN] {
        // SAFETY: installing a trivial (empty, async-signal-safe) handler with a
        // zeroed sigaction whose mask we clear and SA_RESTART set.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = noop as *const () as usize;
            libc::sigemptyset(&raw mut sa.sa_mask);
            sa.sa_flags = libc::SA_RESTART;
            libc::sigaction(sig, &raw const sa, std::ptr::null_mut());
        }
    }
}

/// Send `SIGTSTP` to a whole process group (`kill(-pgid, SIGTSTP)`) — the job-
/// control "suspend this job" primitive (site/content/internals/language-conformance-contract.md). Memory-safe; a `SIGTSTP` to
/// an already-stopped group is a harmless no-op. Exposed so hosts/the evaluator
/// can drive suspend/resume without a direct `libc` dependency.
pub fn suspend_group(pgid: i32) {
    // SAFETY: signalling a process group is memory-safe.
    unsafe {
        libc::kill(-pgid, libc::SIGTSTP);
    }
}

/// Send `SIGCONT` to a whole process group — the "resume this job" primitive.
pub fn continue_group(pgid: i32) {
    // SAFETY: signalling a process group is memory-safe.
    unsafe {
        libc::kill(-pgid, libc::SIGCONT);
    }
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
