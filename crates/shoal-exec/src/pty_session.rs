//! Long-lived, agent-driveable PTY sessions with a built-in terminal emulator.
//!
//! Unlike [`crate::pty::run_pty`] — the one-shot, human-facing `interact`
//! path that spawns a child on a PTY, tees it to the *real* terminal, and
//! blocks until the child exits or is stopped — a [`PtySession`] keeps the
//! child + PTY master **alive** as a keyed, long-lived object with no real
//! terminal attached. A reader thread pumps the child's output bytes into a
//! [`vt100::Parser`], which maintains a `cols×rows` rendered screen grid; the
//! caller (the kernel, over the wire) writes keystrokes into the master and
//! reads back the *rendered screen* — never a wall of raw escape bytes. This
//! is the "an agent reads a rendered screen" surface (AGENT-SURFACE §10).
//!
//! It reuses the same no-leak reaping discipline as the job-control PtyTee
//! path: every child is its own session/process-group leader (portable-pty
//! calls `setsid`), the child is reaped via our own `waitpid` (never
//! `Child::wait`, whose drop neither waits nor kills), and a `reaped` guard
//! makes teardown idempotent so a pid the kernel may have recycled is never
//! re-signalled. [`PtySession::close`] terminates + reaps; [`Drop`] is the
//! backstop for an abandoned session so nothing leaks.
//!
//! Spawning routes through [`crate::sandbox::apply`] exactly like every other
//! spawn, so a scoped leash [`shoal_leash::SandboxPolicy`] (Landlock/Seatbelt
//! plus a spawn-hash pin) confines the child before exec; the default-
//! permissive principal resolves to no confinement, so the human path is
//! byte-unchanged.
//!
//! Threading model: the reader is a **synchronous, poll-based** thread, not an
//! async task — `shoal-exec` is deliberately "blocking and thread-based (no
//! async runtime)" (see the crate docs), and this mirrors `pty::pump_output`'s
//! poll loop so it shares the same prompt-teardown and EOF semantics.

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use shoal_leash::EnforcementStatus;

use crate::status::{decode_wait_status, waitpid_blocking};
use crate::which::resolve_program;
use crate::{ExecMode, ExecSpec, StdinSpec};

/// How often the reader thread wakes to check for teardown when the child is
/// idle (data reads happen immediately on `POLLIN`). Same cadence the PtyTee
/// pump uses.
const READ_POLL_MS: i32 = 50;

/// Hard bounds on the screen grid an agent may request, so a `pty.open`/
/// `pty.resize` can never allocate an unbounded emulator or produce an
/// unbounded `pty.read` payload. A read is always ≤ `cols×rows` cells.
pub const PTY_MAX_COLS: u16 = 1000;
pub const PTY_MAX_ROWS: u16 = 1000;
pub const PTY_DEFAULT_COLS: u16 = 80;
pub const PTY_DEFAULT_ROWS: u16 = 24;

/// A fully-resolved request to open a long-lived interactive PTY session.
#[derive(Debug, Clone)]
pub struct PtyOpenSpec {
    /// `argv[0]` is the program (resolved via `PATH` from [`PtyOpenSpec::env`]
    /// when it contains no `/`), the rest are its arguments.
    pub argv: Vec<OsString>,
    /// Working directory for the child.
    pub cwd: PathBuf,
    /// The **complete** child environment — nothing is inherited implicitly.
    /// A sane `TERM` is injected if the caller supplies none.
    pub env: Vec<(OsString, OsString)>,
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
    /// Optional OS-enforcement request (TDD §8), applied to the child before
    /// exec by [`crate::sandbox::apply`] just like any other spawn. `None`
    /// (default-permissive) spawns unconfined.
    pub sandbox: Option<shoal_leash::SandboxPolicy>,
}

/// A rendered snapshot of a session's terminal screen (bounded by `cols×rows`).
#[derive(Debug, Clone)]
pub struct ScreenSnapshot {
    pub cols: u16,
    pub rows: u16,
    /// Cursor row/col (0-based, within the grid).
    pub cursor_row: u16,
    pub cursor_col: u16,
    /// Whether the program has hidden the cursor.
    pub cursor_hidden: bool,
    /// One rendered plain-text string per grid row (`rows_text.len() == rows`),
    /// trailing blanks trimmed, no escape sequences, no newlines.
    pub rows_text: Vec<String>,
    /// Whether the *rendered* screen changed since the previous
    /// [`PtySession::read_screen`] call (content hash comparison — bytes that
    /// don't alter the visible grid, e.g. a cursor-position query reply, do
    /// not flip this).
    pub changed: bool,
    /// Whether the child's output stream is still open (the child, or a
    /// descendant holding the slave, is still running). `false` once the child
    /// exited and was reaped.
    pub alive: bool,
    /// Exit code once the child has exited and been reaped (`None` while alive
    /// or if it died to a signal).
    pub exit_status: Option<i32>,
    /// Signal name (e.g. `"SIGTERM"`) if the child died to a signal.
    pub exit_signal: Option<String>,
    /// The child's pid (also its process-group id) — lets a caller verify the
    /// OS-level reap after [`PtySession::close`].
    pub pid: u32,
}

/// State shared between the owning [`PtySession`] and its reader thread.
struct Shared {
    /// The terminal emulator. The reader thread feeds it output bytes; readers
    /// snapshot the rendered screen. One `Mutex` guards both directions.
    parser: Mutex<vt100::Parser>,
    /// Set by the reader thread once the master hit EOF/EIO (the child's slave
    /// fully closed) — the honest "child's terminal stream ended" signal.
    child_exited: AtomicBool,
}

/// A long-lived interactive PTY session: a child on a real PTY whose output is
/// rendered into a `vt100` screen grid and whose input is driven by the caller.
pub struct PtySession {
    /// Retained so the PTY slave stays open and so `resize` can push a new
    /// winsize; never moved into the reader thread.
    master: Box<dyn MasterPty + Send>,
    /// A dup of the master fd used to write keystrokes (a plain dup has no
    /// drop-time side effects, unlike `take_writer`; see `pty::dup_master_fd`).
    writer: File,
    /// Held for ownership/lifetime; the child is reaped via our own `waitpid`,
    /// never `Child::wait` (whose drop neither waits nor kills).
    _child: Box<dyn Child + Send + Sync>,
    shared: Arc<Shared>,
    /// Asks the reader thread to stop (teardown) even while the child is idle.
    reader_stop: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
    pid: libc::pid_t,
    pgid: libc::pid_t,
    cols: u16,
    rows: u16,
    /// Hash of the last rendered screen returned by `read_screen`, for the
    /// `changed` bit.
    last_screen_hash: Option<u64>,
    /// `true` once our own `waitpid` reaped the child, so teardown never
    /// re-signals a possibly-recycled pid.
    reaped: bool,
    exit_status: Option<i32>,
    exit_signal: Option<String>,
    #[allow(dead_code)] // reported to the caller at open; kept for completeness
    enforcement: Option<EnforcementStatus>,
}

impl PtySession {
    /// Spawn `spec`'s program on a fresh PTY and start rendering its output.
    ///
    /// # Errors
    /// Propagates program-resolution ([`io::ErrorKind::NotFound`]), sandbox,
    /// PTY-open, and spawn (`E2BIG`, …) failures as [`io::Error`].
    pub fn open(spec: PtyOpenSpec) -> io::Result<Self> {
        let PtyOpenSpec {
            argv,
            cwd,
            mut env,
            cols,
            rows,
            sandbox,
        } = spec;
        let cols = cols.clamp(1, PTY_MAX_COLS);
        let rows = rows.clamp(1, PTY_MAX_ROWS);

        // A TUI needs a terminal type; inject a sane default if the caller gave
        // none so vim/htop/installers behave rather than falling back to dumb.
        if !env.iter().any(|(k, _)| k == "TERM") {
            env.push((OsString::from("TERM"), OsString::from("xterm-256color")));
        }

        // Route the spawn through the shared sandbox path so a scoped leash
        // policy (Landlock/Seatbelt + spawn-hash pin) wraps `argv` and confines
        // the child before exec, exactly like `run_pty`/`spawn_capture`. `None`
        // sandbox is a pure no-op (argv untouched, `Ok(None)` status).
        let mut exec_spec = ExecSpec {
            argv,
            cwd: cwd.clone(),
            env: env.clone(),
            stdin: StdinSpec::Inherit,
            mode: ExecMode::PtyTee,
            sandbox,
            spill: None,
        };
        let enforcement = crate::sandbox::apply(&mut exec_spec)?;
        let ExecSpec { argv, env, .. } = exec_spec;
        let program = resolve_program(&argv, &env)?;

        // Same E2BIG guard run_pty uses: portable-pty's child-side exec-error
        // report can abort past Linux's per-string limit, so reject it up front.
        #[cfg(target_os = "linux")]
        if argv.iter().any(|arg| arg.as_bytes().len() >= 131_072)
            || env
                .iter()
                .any(|(key, value)| key.as_bytes().len() + value.as_bytes().len() + 1 >= 131_072)
        {
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }

        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
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

        let child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
        drop(pair.slave); // parent must not hold the slave or EOF never arrives
        let pid = child
            .process_id()
            .ok_or_else(|| io::Error::other("pty child reported no pid"))?
            as libc::pid_t;
        // setsid (done by portable-pty in the child) makes it its own session
        // AND process-group leader, so pgid == pid; we never setpgid from the
        // parent (a session leader cannot be moved — EPERM).
        let pgid = pid;

        let master = pair.master;
        let writer = dup_master_fd(master.as_ref())?;
        let reader_file = dup_master_fd(master.as_ref())?;

        let shared = Arc::new(Shared {
            // Zero scrollback: only the visible (bounded) screen is ever read.
            parser: Mutex::new(vt100::Parser::new(rows, cols, 0)),
            child_exited: AtomicBool::new(false),
        });
        let reader_stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let shared = Arc::clone(&shared);
            let reader_stop = Arc::clone(&reader_stop);
            thread::spawn(move || pump_into_parser(reader_file, &shared, &reader_stop))
        };

        Ok(Self {
            master,
            writer,
            _child: child,
            shared,
            reader_stop,
            reader: Some(reader),
            pid,
            pgid,
            cols,
            rows,
            last_screen_hash: None,
            reaped: false,
            exit_status: None,
            exit_signal: None,
            enforcement,
        })
    }

    /// The child's pid (also its process-group id).
    #[must_use]
    #[allow(clippy::cast_sign_loss)] // pids are positive
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    /// The current terminal grid size `(cols, rows)` (post-clamp at open, or as
    /// last resized).
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Write raw bytes to the PTY master (keystrokes for the child). The caller
    /// is responsible for encoding named keys into bytes (see [`named_key`]).
    ///
    /// # Errors
    /// Returns an [`io::Error`] if the master write fails (e.g. the child is
    /// gone).
    pub fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Push a new window size to the child (`SIGWINCH` via `TIOCSWINSZ`) and
    /// resize the emulator's grid to match.
    ///
    /// # Errors
    /// Propagates a PTY resize failure.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let cols = cols.clamp(1, PTY_MAX_COLS);
        let rows = rows.clamp(1, PTY_MAX_ROWS);
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(pty_err)?;
        self.shared
            .parser
            .lock()
            .expect("parser lock")
            .screen_mut()
            .set_size(rows, cols);
        self.cols = cols;
        self.rows = rows;
        // A resized grid is a changed grid; force the next read to report it.
        self.last_screen_hash = None;
        Ok(())
    }

    /// Snapshot the rendered screen (bounded by `cols×rows`), the cursor, and
    /// whether the screen changed since the previous call. Opportunistically
    /// reaps the child if it has exited so `alive`/`exit_*` are accurate and no
    /// zombie lingers before an explicit [`PtySession::close`].
    pub fn read_screen(&mut self) -> ScreenSnapshot {
        // If the reader saw EOF, the child's terminal stream ended: reap it
        // (non-blocking) so its exit is reflected and it never lingers.
        if self.shared.child_exited.load(Ordering::SeqCst)
            && !self.reaped
            && let Some(raw) = reap_nohang(self.pid)
        {
            let (status, signal) = decode_wait_status(raw);
            self.exit_status = status;
            self.exit_signal = signal;
            self.reaped = true;
        }

        let (cursor_row, cursor_col, cursor_hidden, rows_text, hash) = {
            let parser = self.shared.parser.lock().expect("parser lock");
            let screen = parser.screen();
            let (cursor_row, cursor_col) = screen.cursor_position();
            let cursor_hidden = screen.hide_cursor();
            let rows_text: Vec<String> = screen.rows(0, self.cols).collect();
            let mut hasher = DefaultHasher::new();
            screen.contents().hash(&mut hasher);
            (
                cursor_row,
                cursor_col,
                cursor_hidden,
                rows_text,
                hasher.finish(),
            )
        };

        let changed = self.last_screen_hash != Some(hash);
        self.last_screen_hash = Some(hash);

        ScreenSnapshot {
            cols: self.cols,
            rows: self.rows,
            cursor_row,
            cursor_col,
            cursor_hidden,
            rows_text,
            changed,
            alive: !self.reaped,
            exit_status: self.exit_status,
            exit_signal: self.exit_signal.clone(),
            pid: self.pid(),
        }
    }

    /// Terminate the child (if still running) and reap it, then tear down the
    /// reader thread. Idempotent: a session already reaped (child exited and
    /// was collected in `read_screen`) only stops its reader. Returns the
    /// child's `(exit_status, exit_signal)`.
    pub fn close(&mut self) -> (Option<i32>, Option<String>) {
        self.terminate();
        (self.exit_status, self.exit_signal.clone())
    }

    /// Kill-and-reap the child's process group (idempotent via `reaped`), then
    /// join the reader thread. Shared by `close` and `Drop`.
    fn terminate(&mut self) {
        self.reader_stop.store(true, Ordering::SeqCst);
        if !self.reaped {
            // SAFETY: signalling a process group is memory-safe. SIGCONT first
            // so a stopped child can act on the kill.
            unsafe {
                libc::kill(-self.pgid, libc::SIGCONT);
                libc::kill(-self.pgid, libc::SIGKILL);
            }
            if let Ok(raw) = waitpid_blocking(self.pid) {
                let (status, signal) = decode_wait_status(raw);
                self.exit_status = status;
                self.exit_signal = signal;
            }
            self.reaped = true;
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Backstop: an abandoned session (kernel dropped it without close, or
        // process shutdown) must never leave the child running/orphaned.
        self.terminate();
    }
}

/// The reader loop: drain the PTY master (a dup'd `reader` fd) and feed every
/// byte into the `vt100` parser so the screen stays current. Poll-based so a
/// teardown request (`reader_stop`) tears it down promptly when the child is
/// idle; otherwise it exits at EOF (`read` → 0 / `EIO`, the slave closing),
/// recording that the child's stream ended.
fn pump_into_parser(reader: File, shared: &Shared, reader_stop: &AtomicBool) {
    let fd = reader.as_raw_fd();
    let mut buf = [0u8; 8192];
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll on one valid pollfd we own.
        let n = unsafe { libc::poll(&raw mut pfd, 1, READ_POLL_MS) };
        if n <= 0 {
            if reader_stop.load(Ordering::SeqCst) {
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
                Some(v) if v == libc::EIO => break, // slave closed — treat as EOF
                Some(v) if v == libc::EINTR || v == libc::EAGAIN => continue,
                _ => break,
            }
        }
        #[allow(clippy::cast_sign_loss)] // r > 0 checked above
        let n = r as usize;
        shared
            .parser
            .lock()
            .expect("parser lock")
            .process(&buf[..n]);
    }
    shared.child_exited.store(true, Ordering::SeqCst);
}

/// A `File` onto the PTY master via a dup'd fd (no drop-time side effects,
/// unlike `MasterPty::take_writer`). Mirrors `pty::dup_master_fd`, duplicated
/// here so the human PtyTee path in `pty.rs` stays untouched.
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

/// Non-blocking reap: collect the child if it has become a zombie, else leave
/// it. `Some(raw_status)` when it was reaped here.
fn reap_nohang(pid: libc::pid_t) -> Option<i32> {
    let mut status: libc::c_int = 0;
    // SAFETY: waitpid with a valid out-pointer; WNOHANG never blocks.
    let r = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
    (r == pid).then_some(status)
}

fn pty_err(e: anyhow::Error) -> io::Error {
    match e.downcast::<io::Error>() {
        Ok(error) => error,
        Err(error) => io::Error::other(error.to_string()),
    }
}

/// Encode a named key into the bytes a terminal sends for it, or `None` for an
/// unrecognized name. This is the terminal-domain half of the `pty.send`
/// key-name protocol (AGENT-SURFACE §10); the JSON shape of a send request is
/// decoded by the kernel, which calls this per named key.
///
/// Recognized (case-insensitive for the names, exact for `Ctrl-<x>`):
/// `Enter`/`Return`, `Tab`, `Escape`/`Esc`, `Backspace`, `Delete`/`Del`,
/// `Space`, `Up`/`Down`/`Right`/`Left`, `Home`, `End`, `PageUp`/`PageDown`,
/// `F1`..`F12`, and `Ctrl-<letter>` / `C-<letter>` (also `Ctrl-[`, `Ctrl-\`,
/// `Ctrl-]`, `Ctrl-Space`).
#[must_use]
pub fn named_key(name: &str) -> Option<Vec<u8>> {
    let bytes: &[u8] = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" | "cr" => b"\r",
        "newline" | "lf" => b"\n",
        "tab" => b"\t",
        "backtab" | "shift-tab" => b"\x1b[Z",
        "escape" | "esc" => b"\x1b",
        "backspace" | "bs" => b"\x7f",
        "delete" | "del" => b"\x1b[3~",
        "space" => b" ",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" | "pgup" => b"\x1b[5~",
        "pagedown" | "pgdn" => b"\x1b[6~",
        "insert" | "ins" => b"\x1b[2~",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        _ => return control_key(name),
    };
    Some(bytes.to_vec())
}

/// Parse `Ctrl-<x>` / `C-<x>` control combinations into their single control
/// byte. `Ctrl-A`..`Ctrl-Z` → 0x01..0x1a; plus the punctuation controls.
fn control_key(name: &str) -> Option<Vec<u8>> {
    let rest = name
        .strip_prefix("Ctrl-")
        .or_else(|| name.strip_prefix("ctrl-"))
        .or_else(|| name.strip_prefix("C-"))?;
    let byte = match rest {
        "Space" | "space" | "@" => 0x00,
        "[" => 0x1b,
        "\\" => 0x1c,
        "]" => 0x1d,
        "^" => 0x1e,
        "_" => 0x1f,
        "?" => 0x7f,
        s if s.len() == 1 => {
            let c = s.as_bytes()[0];
            if c.is_ascii_alphabetic() {
                (c.to_ascii_uppercase() - b'A') + 1
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(vec![byte])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn spec(argv: &[&str], cols: u16, rows: u16) -> PtyOpenSpec {
        PtyOpenSpec {
            argv: argv.iter().map(OsString::from).collect(),
            cwd: std::env::current_dir().expect("cwd"),
            env: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            cols,
            rows,
            sandbox: None,
        }
    }

    #[allow(clippy::cast_possible_wrap)]
    fn process_is_gone(pid: u32) -> bool {
        let pid = pid as libc::pid_t;
        // SAFETY: signal 0 is the POSIX existence probe; it delivers nothing.
        unsafe {
            libc::kill(pid, 0) == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        }
    }

    fn read_until(session: &mut PtySession, needle: &str, timeout: Duration) -> ScreenSnapshot {
        let deadline = Instant::now() + timeout;
        loop {
            let snap = session.read_screen();
            if snap.rows_text.iter().any(|r| r.contains(needle)) {
                return snap;
            }
            assert!(
                Instant::now() < deadline,
                "screen never showed {needle:?}; got {:?}",
                snap.rows_text
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The core vertical slice at the crate boundary: drive `cat` (a
    /// line-oriented echo program), send text + a named Enter, read back the
    /// RENDERED screen and see the echoed text, then close and prove the child
    /// is reaped (no zombie/orphan).
    #[test]
    fn cat_echoes_typed_line_into_the_rendered_screen_then_closes_clean() {
        let mut session = PtySession::open(spec(&["cat"], 80, 24)).expect("open cat");
        let pid = session.pid();

        session.send(b"hello-pty").expect("send text");
        session
            .send(&named_key("Enter").expect("Enter key"))
            .expect("send enter");

        let snap = read_until(&mut session, "hello-pty", Duration::from_secs(5));
        assert!(
            snap.rows_text.iter().any(|r| r.contains("hello-pty")),
            "rendered screen must show the echoed line: {:?}",
            snap.rows_text
        );
        // The cursor advanced off row 0 — the emulator tracked the newline.
        assert!(snap.cursor_row >= 1, "cursor should have moved down");
        assert!(snap.alive, "cat is still running");

        let (status, signal) = session.close();
        // We SIGKILL'd it, so it dies to a signal (no clean exit code).
        assert!(
            status.is_none() || signal.is_some(),
            "a killed child reports a signal death"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process_is_gone(pid) {
            assert!(
                Instant::now() < deadline,
                "child must be reaped after close"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// A child that exits on its own is observed as not-alive and reaped by
    /// `read_screen` (no explicit close needed), leaving no zombie.
    #[test]
    fn self_exiting_child_is_observed_and_reaped_without_leak() {
        let mut session =
            PtySession::open(spec(&["sh", "-c", "printf done; exit 0"], 80, 24)).expect("open sh");
        let pid = session.pid();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snap = session.read_screen();
            if !snap.alive {
                assert_eq!(snap.exit_status, Some(0), "clean exit reported");
                break;
            }
            assert!(Instant::now() < deadline, "child never observed as exited");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(process_is_gone(pid), "self-exited child must be reaped");
        // Drop is a safe no-op on an already-reaped session.
        drop(session);
    }

    /// The `changed` bit: a read after new output is `changed`, an immediate
    /// re-read with no new output is not.
    #[test]
    fn changed_bit_tracks_new_output() {
        let mut session = PtySession::open(spec(&["cat"], 80, 24)).expect("open cat");
        session.send(b"abc\r").expect("send");
        let first = read_until(&mut session, "abc", Duration::from_secs(5));
        assert!(first.changed, "first sighting of new output is a change");
        // No new input; the next read sees an unchanged screen.
        thread::sleep(Duration::from_millis(80));
        let second = session.read_screen();
        assert!(!second.changed, "no new output ⇒ not changed");
        session.close();
    }

    #[test]
    fn named_keys_encode_expected_bytes() {
        assert_eq!(named_key("Enter"), Some(b"\r".to_vec()));
        assert_eq!(named_key("Escape"), Some(b"\x1b".to_vec()));
        assert_eq!(named_key("Tab"), Some(b"\t".to_vec()));
        assert_eq!(named_key("Backspace"), Some(b"\x7f".to_vec()));
        assert_eq!(named_key("Up"), Some(b"\x1b[A".to_vec()));
        assert_eq!(named_key("Ctrl-C"), Some(vec![0x03]));
        assert_eq!(named_key("ctrl-a"), Some(vec![0x01]));
        assert_eq!(named_key("C-d"), Some(vec![0x04]));
        assert_eq!(named_key("Ctrl-["), Some(vec![0x1b]));
        assert_eq!(named_key("nonsense-key"), None);
    }
}
