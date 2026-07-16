//! PtyTee mode: the child runs on a real PTY as session leader; output
//! streams raw to the real terminal and is teed into the result buffer.
//!
//! Job control (TDD §4.7) is layered on top of this without disturbing the
//! passthrough. Every PtyTee child is a session/process-group leader (portable-
//! pty calls `setsid` in the child, so its process-group id equals its pid — we
//! never `setpgid` from the parent, which would `EPERM` on a session leader).
//! A **foreground** child is waited on with `WUNTRACED`, so a Ctrl-Z (the pty
//! line discipline turns the forwarded `^Z` byte into `SIGTSTP` for the child's
//! foreground process group) surfaces as a *stop* instead of hanging the shell.
//! On a stop, the still-live PTY (master + child) is packaged into a [`PtyJob`]
//! and parked so the host can resume it in the foreground (`fg`) or background
//! (`bg`). A child that simply runs to completion behaves byte-for-byte as it
//! did before job control existed.
//!
//! **Terminal handoff.** In this PTY-tee model the child owns *its own* pty as
//! session leader; the shell owns the *real* terminal. They are on different
//! terminals, so the classic `tcsetpgrp(real_tty, child_pgid)` dance does not
//! apply (the child is not in the real terminal's session — such a call would
//! `EPERM`/`ENOTTY`). The effective "give the child the terminal" step here is
//! engaging raw mode on the real terminal and forwarding its input to the pty;
//! "reclaim the terminal" is restoring cooked mode (the [`RawModeGuard`] drop).
//! The shell must additionally ignore `SIGTTOU`/`SIGTTIN`/`SIGTSTP` so those
//! terminal-control operations never suspend the shell itself — that is the
//! host's job and the REPL installs those dispositions (see `crates/shoal/
//! src/repl.rs`); children still get the default disposition because `exec`
//! resets caught signals to `SIG_DFL`, which is exactly why Ctrl-Z can stop
//! them.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use shoal_leash::EnforcementStatus;

use crate::cancel::CancelToken;
use crate::status::{decode_wait_status, is_stopped, waitpid_blocking, waitpid_untraced};
use crate::watcher::spawn_cancel_watcher;
use crate::which::resolve_program;
use crate::{ExecResult, ExecSpec, StdinSpec};

/// How often the stdin forwarder / output pump wake to poll (also paces winsize
/// checks). Data reads happen immediately on `POLLIN`; this only bounds the
/// latency of noticing a teardown request when the pty is idle.
const INPUT_POLL_MS: i32 = 50;
/// Winsize is re-checked every N input polls (≈ every 200 ms).
const WINSIZE_EVERY_N_POLLS: u32 = 4;
/// After the child is reaped, how long we wait for the output pump to hit
/// EOF before abandoning it (it exits on its own once the pty closes).
const PUMP_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Parked, still-running PTY foreground jobs that were stopped (Ctrl-Z /
/// SIGTSTP) and can be resumed by the host via `fg`/`bg`. Process-global
/// because job control is inherently a per-process singleton (there is one
/// controlling terminal). Keyed by pid at [`take_stopped_job`].
static PARKED_JOBS: Mutex<VecDeque<PtyJob>> = Mutex::new(VecDeque::new());

fn pty_err(e: anyhow::Error) -> io::Error {
    // portable-pty wraps operating-system failures in anyhow. Preserve the
    // original io::Error so callers can reliably distinguish ENOENT/E2BIG/etc.
    match e.downcast::<io::Error>() {
        Ok(error) => error,
        Err(error) => io::Error::other(error.to_string()),
    }
}

/// Restores the original termios of `fd` on drop — including on panic, so
/// the user's terminal is never left in raw mode.
struct RawModeGuard {
    fd: RawFd,
    orig: libc::termios,
}

impl RawModeGuard {
    /// Put `fd` into raw mode; `None` if it is not a tty or termios fails.
    fn new(fd: RawFd) -> Option<Self> {
        // SAFETY: termios syscalls on a caller-owned fd with valid pointers.
        unsafe {
            let mut term = mem::zeroed::<libc::termios>();
            if libc::tcgetattr(fd, &raw mut term) != 0 {
                return None;
            }
            let orig = term;
            libc::cfmakeraw(&raw mut term);
            if libc::tcsetattr(fd, libc::TCSANOW, &raw const term) != 0 {
                return None;
            }
            Some(RawModeGuard { fd, orig })
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // SAFETY: restoring the termios we captured in `new`.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &raw const self.orig);
        }
    }
}

/// Current window size of the tty on `fd`, if it is one.
fn tty_winsize(fd: RawFd) -> Option<PtySize> {
    // SAFETY: TIOCGWINSZ with a valid winsize out-pointer.
    unsafe {
        let mut ws = mem::zeroed::<libc::winsize>();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &raw mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            Some(PtySize {
                rows: ws.ws_row,
                cols: ws.ws_col,
                pixel_width: ws.ws_xpixel,
                pixel_height: ws.ws_ypixel,
            })
        } else {
            None
        }
    }
}

/// Push `sz` onto the pty whose master (or a dup of it) is `fd` via
/// `TIOCSWINSZ` — the fd-only equivalent of `MasterPty::resize`, so the stdin
/// forwarder can propagate resizes using only its dup'd writer fd (leaving the
/// `MasterPty` owned by the job for resumption).
fn set_winsize(fd: RawFd, sz: PtySize) {
    let ws = libc::winsize {
        ws_row: sz.rows,
        ws_col: sz.cols,
        ws_xpixel: sz.pixel_width,
        ws_ypixel: sz.pixel_height,
    };
    // SAFETY: TIOCSWINSZ with a valid winsize pointer on a pty fd we own.
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &raw const ws);
    }
}

/// A dup of fd 1 for raw byte passthrough (bypasses std's line buffering and
/// leaves the real fd 1 open when dropped).
fn dup_stdout() -> Option<File> {
    // SAFETY: dup returns a fresh fd we then own; from_raw_fd takes it over.
    let fd = unsafe { libc::dup(1) };
    if fd < 0 {
        None
    } else {
        Some(unsafe { File::from_raw_fd(fd) })
    }
}

/// A `File` onto the pty master via a dup'd fd, usable for both reading the
/// child's output and writing its input.
///
/// Deliberately NOT `MasterPty::take_writer()`: portable-pty's writer injects
/// `"\n" + VEOF` into the pty when dropped, and the line discipline echoes
/// that back as a stray `\r\n` in the teed output. A plain dup has no
/// drop-time side effects; pty EOF, when wanted, must be conveyed in-band
/// (a VEOF byte, usually `0x04`) by whoever feeds the input. Dup'ing (rather
/// than moving the `MasterPty`) is also what lets the job keep the master alive
/// across a stop so it can be resumed.
fn dup_master_fd(master: &dyn MasterPty) -> io::Result<File> {
    let fd = master
        .as_raw_fd()
        .ok_or_else(|| io::Error::other("pty master has no raw fd"))?;
    // SAFETY: dup returns a fresh fd we then own; from_raw_fd takes it over.
    let dup = unsafe { libc::dup(fd) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(dup) })
}

/// Forward real-stdin bytes to the pty master and propagate window resizes.
///
/// Uses poll(2) with a short timeout instead of a blocking read so that once
/// `done` is set (the serve loop is tearing down) the thread exits without
/// stealing a keystroke that belongs to the shell. Resizes are pushed through
/// the writer fd (a dup of the master) via `TIOCSWINSZ`, so this needs no
/// reference to the `MasterPty` object — that stays owned by the job.
fn forward_stdin_and_resize(mut writer: File, done: &AtomicBool) {
    let wfd = writer.as_raw_fd();
    let mut buf = [0u8; 4096];
    let mut last = tty_winsize(0);
    let mut ticks: u32 = 0;
    while !done.load(Ordering::SeqCst) {
        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(WINSIZE_EVERY_N_POLLS)
            && let Some(sz) = tty_winsize(0)
        {
            let changed = last.is_none_or(|l| l.rows != sz.rows || l.cols != sz.cols);
            if changed {
                set_winsize(wfd, sz);
                last = Some(sz);
            }
        }
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll on one valid pollfd.
        let n = unsafe { libc::poll(&raw mut pfd, 1, INPUT_POLL_MS) };
        if n <= 0 || pfd.revents & (libc::POLLIN | libc::POLLHUP) == 0 {
            continue;
        }
        // SAFETY: read into a valid buffer of the stated length.
        let r = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if r <= 0 {
            break; // stdin EOF or error
        }
        #[allow(clippy::cast_sign_loss)] // r > 0 checked above
        let n = r as usize;
        if writer.write_all(&buf[..n]).is_err() {
            break; // pty gone (child exited)
        }
        let _ = writer.flush();
    }
}

/// The output pump: drain the pty master (a dup'd `reader` fd), tee into the
/// bounded capture buffer, and — when the real terminal is a tty — pass the
/// bytes through raw. Poll-based so a *stop* (child suspended, no EOF) can tear
/// it down promptly: when the pty is idle and `serve_done` is set, the pump
/// returns; otherwise it exits at EOF (`read` -> 0 / `EIO`, the slave closing).
/// The tee is capped to [`crate::capture_hard_cap`] so a runaway child cannot
/// OOM the shell; the real terminal still receives the full stream.
fn pump_output(
    reader: File,
    tee: Arc<Mutex<Vec<u8>>>,
    tee_truncated: Arc<AtomicBool>,
    mut passthrough: Option<File>,
    cap: usize,
    serve_done: Arc<AtomicBool>,
    pump_done: Arc<AtomicBool>,
) {
    let fd = reader.as_raw_fd();
    let mut buf = [0u8; 8192];
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll on one valid pollfd we own.
        let n = unsafe { libc::poll(&raw mut pfd, 1, INPUT_POLL_MS) };
        if n <= 0 {
            // Timed out or was interrupted with no data ready: exit if the
            // serve loop asked us to (a stop, where no EOF will ever arrive),
            // otherwise keep waiting for the child's next output.
            if serve_done.load(Ordering::SeqCst) {
                break;
            }
            continue;
        }
        if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
            continue;
        }
        // SAFETY: read into a valid buffer of the stated length.
        let r = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if r == 0 {
            break; // EOF: slave fully closed
        }
        if r < 0 {
            match io::Error::last_os_error().raw_os_error() {
                // EIO: the slave side has been closed — treat as EOF, exactly as
                // portable-pty's own reader does.
                Some(v) if v == libc::EIO => break,
                Some(v) if v == libc::EINTR || v == libc::EAGAIN => continue,
                _ => break,
            }
        }
        #[allow(clippy::cast_sign_loss)] // r > 0 checked above
        let n = r as usize;
        {
            let mut tee = tee.lock().expect("tee lock");
            if tee.len() < cap {
                let take = (cap - tee.len()).min(n);
                tee.extend_from_slice(&buf[..take]);
                if take < n {
                    tee_truncated.store(true, Ordering::SeqCst);
                }
            } else {
                tee_truncated.store(true, Ordering::SeqCst);
            }
        }
        if let Some(out) = passthrough.as_mut() {
            let _ = out.write_all(&buf[..n]);
        }
    }
    pump_done.store(true, Ordering::SeqCst);
}

/// Non-tty stdin to feed into the pty exactly once (the first time the job is
/// served). `Inherit`-tty forwarding is handled separately and re-engages on
/// every foreground serve.
enum Feed {
    Bytes(Vec<u8>),
    File(File),
}

/// The result of serving (waiting on) a PTY child for one foreground/background
/// stint: it either terminated (raw wait status) or *stopped* (suspended).
enum Wait {
    Exited(i32),
    Stopped,
}

/// A PTY foreground command and everything needed to keep it alive across a
/// stop and later resume it (TDD §4.7). Created for every PtyTee run; parked in
/// [`PARKED_JOBS`] only when the child is stopped rather than finishing.
///
/// The `master` is retained (never moved into a helper thread) precisely so the
/// child's pty slave stays open while it is stopped — dropping the master would
/// `SIGHUP` the child on resume. On drop without a clean reap, the whole group
/// is continued-then-killed so no stopped child is orphaned.
pub struct PtyJob {
    master: Box<dyn MasterPty + Send>,
    /// Kept for ownership/lifetime; the child is reaped via our own `waitpid`
    /// (`std::process::Child`'s drop neither waits nor kills), never `.wait()`.
    _child: Box<dyn Child + Send + Sync>,
    pid: libc::pid_t,
    pgid: libc::pid_t,
    tee: Arc<Mutex<Vec<u8>>>,
    tee_truncated: Arc<AtomicBool>,
    /// Whether a real interactive terminal is being forwarded to the child.
    forward_tty: bool,
    /// One-shot non-tty stdin, consumed on the first serve.
    pending_feed: Option<Feed>,
    start: Instant,
    enforcement: Option<EnforcementStatus>,
    cap: usize,
    display: String,
    stdout_is_tty: bool,
    /// `true` once our own `waitpid` has reaped the child, so `Drop`/`kill_and_
    /// reap` know not to signal a dead (possibly pid-reused) process.
    reaped: bool,
}

impl PtyJob {
    /// The child's pid (also its process-group id).
    #[must_use]
    #[allow(clippy::cast_sign_loss)] // pids are positive
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    /// The child's process-group id (`kill(-pgid, …)` reaches the whole job).
    #[must_use]
    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// The command's display form, for a jobs listing.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.display
    }

    /// `SIGCONT` the whole process group.
    fn signal_cont(&self) {
        // SAFETY: signalling a process group is memory-safe.
        unsafe {
            libc::kill(-self.pgid, libc::SIGCONT);
        }
    }

    /// Attach helpers (stdin forward + output pump + cancel watcher), optionally
    /// `SIGCONT` the group (on resume), then wait — observing a stop. Keeps the
    /// master and child alive on every path.
    fn serve(&mut self, cancel: &CancelToken, foreground: bool, resume: bool) -> io::Result<Wait> {
        // Raw mode only when actually forwarding a real terminal; restored on
        // every exit path (panics included) when the guard drops.
        let _raw = if foreground && self.forward_tty {
            RawModeGuard::new(0)
        } else {
            None
        };

        let serve_done = Arc::new(AtomicBool::new(false));
        let mut helpers: Vec<thread::JoinHandle<()>> = Vec::new();

        // Stdin plumbing. One-shot Bytes/File feed happens on the first serve;
        // tty forwarding re-engages on every foreground serve.
        if let Some(feed) = self.pending_feed.take() {
            let mut w = dup_master_fd(self.master.as_ref())?;
            helpers.push(thread::spawn(move || match feed {
                Feed::Bytes(bytes) => {
                    let _ = w.write_all(&bytes);
                    let _ = w.flush();
                }
                Feed::File(mut f) => {
                    let _ = io::copy(&mut f, &mut w);
                    let _ = w.flush();
                }
            }));
        } else if foreground && self.forward_tty {
            let w = dup_master_fd(self.master.as_ref())?;
            let d = serve_done.clone();
            helpers.push(thread::spawn(move || forward_stdin_and_resize(w, &d)));
        }

        // Output pump (poll-based, over a dup of the master fd so it can be torn
        // down on a stop without dropping the master itself).
        let reader = dup_master_fd(self.master.as_ref())?;
        let pump_done = Arc::new(AtomicBool::new(false));
        let pump = {
            let tee = Arc::clone(&self.tee);
            let tee_truncated = Arc::clone(&self.tee_truncated);
            let pump_done = Arc::clone(&pump_done);
            let serve_done = Arc::clone(&serve_done);
            let passthrough = if self.stdout_is_tty {
                dup_stdout()
            } else {
                None
            };
            let cap = self.cap;
            thread::spawn(move || {
                pump_output(
                    reader,
                    tee,
                    tee_truncated,
                    passthrough,
                    cap,
                    serve_done,
                    pump_done,
                );
            })
        };

        // Cancellation watcher (INT → TERM → KILL against the child's group).
        let claimed = Arc::new(AtomicBool::new(false));
        let watcher =
            spawn_cancel_watcher(self.pgid, vec![cancel.clone()], claimed, serve_done.clone());

        // On resume, continue the group now that output/input are re-attached
        // (spawning the pump first means no output is lost to the gap).
        if resume {
            self.signal_cont();
        }

        // Wait, observing stops (WUNTRACED). A stop does NOT reap the child.
        let raw = waitpid_untraced(self.pid)?;
        let stopped = is_stopped(raw);

        // Tear down this serve's helpers. Identical for stop and exit — the
        // master and child are always left intact; on a stop they are parked.
        serve_done.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + PUMP_DRAIN_GRACE;
        while !pump_done.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if pump_done.load(Ordering::SeqCst) {
            let _ = pump.join();
        }
        // If a grandchild is holding the slave open and flooding it, the pump
        // may not have drained within the grace window; leave it (it exits on
        // its own once serve_done is observed on the next idle poll) rather than
        // block the prompt — the same policy the pre-job-control code used.
        let _ = watcher.join();
        for h in helpers {
            let _ = h.join();
        }

        Ok(if stopped {
            Wait::Stopped
        } else {
            Wait::Exited(raw)
        })
    }

    /// Build the terminal-exit result and mark the child reaped.
    fn exit_result(&mut self, raw: i32) -> ExecResult {
        let (status, signal) = decode_wait_status(raw);
        let stdout = mem::take(&mut *self.tee.lock().expect("tee lock"));
        let truncated = self.tee_truncated.load(Ordering::SeqCst);
        self.reaped = true;
        #[allow(clippy::cast_sign_loss)] // pids are positive
        ExecResult {
            status,
            signal,
            stdout,
            stderr: Vec::new(),
            truncated,
            // PtyTee (statement position) never spills to disk: its bytes
            // already reached the real terminal and the tee is a bounded
            // convenience — §317 value-position spill is a Capture-mode concern.
            stdout_spill: None,
            dur: self.start.elapsed(),
            pid: self.pid as u32,
            pgid: self.pgid as u32,
            stopped: false,
            enforcement: self.enforcement.take(),
        }
    }

    /// Build the *stopped* result (child alive, suspended). Snapshots the tee
    /// (clone, not take) so a later resume keeps appending to the same buffer.
    fn stopped_result(&self) -> ExecResult {
        let stdout = self.tee.lock().expect("tee lock").clone();
        let truncated = self.tee_truncated.load(Ordering::SeqCst);
        #[allow(clippy::cast_sign_loss)] // pids are positive
        ExecResult {
            status: None,
            signal: None,
            stdout,
            stderr: Vec::new(),
            truncated,
            // A stopped PtyTee job is not a value-position capture; no spill.
            stdout_spill: None,
            dur: self.start.elapsed(),
            pid: self.pid as u32,
            pgid: self.pgid as u32,
            stopped: true,
            enforcement: self.enforcement.clone(),
        }
    }

    /// Continue-then-kill the whole group and reap, for an abandoned job.
    fn kill_and_reap(&mut self) {
        if self.reaped {
            return;
        }
        // SAFETY: signalling a process group is memory-safe. SIGCONT first so a
        // stopped child can actually act on the kill.
        unsafe {
            libc::kill(-self.pgid, libc::SIGCONT);
            libc::kill(-self.pgid, libc::SIGKILL);
        }
        let _ = waitpid_blocking(self.pid);
        self.reaped = true;
    }

    /// Resume this job in the **foreground** (`fg`): `SIGCONT`, re-attach the
    /// terminal, and wait again with `WUNTRACED`. Returns the terminal result
    /// when the child finishes; if it stops again, it is re-parked and the
    /// returned [`ExecResult`] has `stopped: true`.
    ///
    /// # Errors
    /// Propagates a `waitpid`/pty-plumbing [`io::Error`].
    pub fn resume_foreground(mut self, cancel: &CancelToken) -> io::Result<ExecResult> {
        match self.serve(cancel, true, true)? {
            Wait::Exited(raw) => Ok(self.exit_result(raw)),
            Wait::Stopped => {
                let res = self.stopped_result();
                park_job(self);
                Ok(res)
            }
        }
    }

    /// Resume this job in the **background** (`bg`): `SIGCONT` and let it run
    /// detached. A background pump keeps teeing the child's output to the real
    /// terminal and reaps it when it finally exits; stdin is not forwarded (a
    /// background job does not own the terminal input). A background job is not
    /// cancelled by the foreground Ctrl-C, so it runs under its own fresh,
    /// never-tripped [`CancelToken`].
    pub fn resume_background(mut self) {
        self.forward_tty = false;
        thread::spawn(move || {
            let cancel = CancelToken::new();
            match self.serve(&cancel, false, true) {
                Ok(Wait::Exited(raw)) => {
                    let _ = self.exit_result(raw);
                }
                // Stopped again in the background (unusual): re-park so a later
                // `fg` can still find it.
                Ok(Wait::Stopped) => park_job(self),
                Err(_) => self.kill_and_reap(),
            }
        });
    }
}

impl Drop for PtyJob {
    fn drop(&mut self) {
        // A job dropped without a clean reap — e.g. a parked job abandoned when
        // the shell exits — must not leave a stopped child orphaned.
        self.kill_and_reap();
    }
}

/// Park a stopped job so the host can later resume it by pid.
fn park_job(job: PtyJob) {
    if let Ok(mut jobs) = PARKED_JOBS.lock() {
        jobs.push_back(job);
    }
}

/// Remove and return the parked (stopped) PTY job for `pid`, if any. The host
/// (REPL) calls this to resume a Ctrl-Z'd foreground command via `fg`/`bg`.
#[must_use]
pub fn take_stopped_job(pid: u32) -> Option<PtyJob> {
    let mut jobs = PARKED_JOBS.lock().ok()?;
    let idx = jobs.iter().position(|j| j.pid() == pid)?;
    jobs.remove(idx)
}

/// Drain every parked job, killing and reaping each (via `PtyJob::drop`). The
/// host calls this on shutdown so stopped children are not left orphaned when
/// the shell exits (statics are not dropped at process exit).
pub fn shutdown_stopped_jobs() {
    if let Ok(mut jobs) = PARKED_JOBS.lock() {
        jobs.clear();
    }
}

/// Run `spec` on a real PTY, teeing the merged output stream. In interactive
/// foreground use the child may be *stopped* (Ctrl-Z) instead of finishing, in
/// which case the returned [`ExecResult`] has `stopped: true` and the live PTY
/// is parked for resumption (see the module docs and [`take_stopped_job`]).
pub(crate) fn run_pty(mut spec: ExecSpec, cancel: &CancelToken) -> io::Result<ExecResult> {
    let enforcement = crate::sandbox::apply(&mut spec)?;
    let ExecSpec {
        argv,
        cwd,
        env,
        stdin,
        ..
    } = spec;
    let program = resolve_program(&argv, &env)?;

    // portable-pty's Unix fork helper currently aborts in the child when its
    // exec-error report itself cannot be written after E2BIG. Reject Linux's
    // fixed per-string limit up front so E2BIG remains an ordinary io error.
    #[cfg(target_os = "linux")]
    if argv.iter().any(|arg| arg.as_bytes().len() >= 131_072)
        || env
            .iter()
            .any(|(key, value)| key.as_bytes().len() + value.as_bytes().len() + 1 >= 131_072)
    {
        return Err(io::Error::from_raw_os_error(libc::E2BIG));
    }

    // SAFETY: isatty is a trivial fd query.
    let stdin_is_tty = unsafe { libc::isatty(0) } == 1;
    let stdout_is_tty = unsafe { libc::isatty(1) } == 1;

    // Open a File stdin source before spawning so errors surface early.
    let stdin_file = match &stdin {
        StdinSpec::File(p) => Some(File::open(p)?),
        _ => None,
    };

    let size = tty_winsize(0)
        .or_else(|| tty_winsize(1))
        .unwrap_or(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });
    let pty = native_pty_system();
    let pair = pty.openpty(size).map_err(pty_err)?;

    let mut cmd = CommandBuilder::new(&program);
    for a in &argv[1..] {
        cmd.arg(a);
    }
    cmd.cwd(&cwd);
    cmd.env_clear();
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let display = argv
        .iter()
        .map(|x| x.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");

    let start = Instant::now();
    // portable-pty makes the child a session leader with the slave as its
    // controlling tty; E2BIG and friends surface here as io errors.
    let child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
    drop(pair.slave); // parent must not hold the slave or EOF never arrives
    let pid = child
        .process_id()
        .ok_or_else(|| io::Error::other("pty child reported no pid"))? as libc::pid_t;
    // setsid (done by portable-pty in the child) makes the child its own
    // session AND process-group leader, so its pgid is its pid. We deliberately
    // do NOT setpgid from the parent: a session leader cannot be moved to
    // another group (EPERM), and setsid already gives the isolated group job
    // control needs.
    let pgid = pid;

    let forward_tty = stdin_is_tty && matches!(stdin, StdinSpec::Inherit);
    let pending_feed = match stdin {
        StdinSpec::Bytes(bytes) => Some(Feed::Bytes(bytes)),
        StdinSpec::File(_) => Some(Feed::File(
            stdin_file.expect("opened above for StdinSpec::File"),
        )),
        StdinSpec::Null | StdinSpec::Inherit => None,
    };

    let mut job = PtyJob {
        master: pair.master,
        _child: child,
        pid,
        pgid,
        tee: Arc::new(Mutex::new(Vec::new())),
        tee_truncated: Arc::new(AtomicBool::new(false)),
        forward_tty,
        pending_feed,
        start,
        enforcement,
        cap: crate::capture_hard_cap(),
        display,
        stdout_is_tty,
        reaped: false,
    };

    match job.serve(cancel, true, false) {
        Ok(Wait::Exited(raw)) => Ok(job.exit_result(raw)),
        Ok(Wait::Stopped) => {
            let res = job.stopped_result();
            park_job(job);
            Ok(res)
        }
        Err(e) => {
            job.kill_and_reap();
            Err(e)
        }
    }
}
