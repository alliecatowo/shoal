//! PtyTee mode: the child runs on a real PTY as session leader; output
//! streams raw to the real terminal and is teed into the result buffer.

use std::fmt::Display;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::cancel::CancelToken;
use crate::status::{decode_wait_status, waitpid_blocking};
use crate::watcher::spawn_cancel_watcher;
use crate::which::resolve_program;
use crate::{ExecResult, ExecSpec, StdinSpec};

/// How often the stdin forwarder wakes to poll (also paces winsize checks).
const INPUT_POLL_MS: i32 = 50;
/// Winsize is re-checked every N input polls (≈ every 200 ms).
const WINSIZE_EVERY_N_POLLS: u32 = 4;
/// After the child is reaped, how long we wait for the output pump to hit
/// EOF before abandoning it (it exits on its own once the pty closes).
const PUMP_DRAIN_GRACE: Duration = Duration::from_millis(500);

fn pty_err(e: impl Display) -> io::Error {
    io::Error::other(e.to_string())
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

/// A writer onto the pty master via a dup'd fd.
///
/// Deliberately NOT `MasterPty::take_writer()`: portable-pty's writer injects
/// `"\n" + VEOF` into the pty when dropped, and the line discipline echoes
/// that back as a stray `\r\n` in the teed output. A plain dup has no
/// drop-time side effects; pty EOF, when wanted, must be conveyed in-band
/// (a VEOF byte, usually `0x04`) by whoever feeds the input.
fn dup_master_writer(master: &dyn MasterPty) -> io::Result<File> {
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
/// `done` is set (child reaped) the thread exits without stealing a keystroke
/// that belongs to the shell.
fn forward_stdin_and_resize(
    mut writer: File,
    master: Box<dyn MasterPty + Send>,
    done: &AtomicBool,
) {
    let mut buf = [0u8; 4096];
    let mut last = tty_winsize(0);
    let mut ticks: u32 = 0;
    while !done.load(Ordering::SeqCst) {
        ticks = ticks.wrapping_add(1);
        if ticks % WINSIZE_EVERY_N_POLLS == 0
            && let Some(sz) = tty_winsize(0)
        {
            let changed = last.is_none_or(|l| l.rows != sz.rows || l.cols != sz.cols);
            if changed {
                let _ = master.resize(sz);
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

/// Run `spec` on a real PTY, teeing the merged output stream.
pub(crate) fn run_pty(spec: ExecSpec, cancel: &CancelToken) -> io::Result<ExecResult> {
    let ExecSpec {
        argv,
        cwd,
        env,
        stdin,
        ..
    } = spec;
    let program = resolve_program(&argv, &env)?;

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

    let start = Instant::now();
    // portable-pty makes the child a session leader with the slave as its
    // controlling tty; E2BIG and friends surface here as io errors.
    let child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
    drop(pair.slave); // parent must not hold the slave or EOF never arrives
    let pid = child
        .process_id()
        .ok_or_else(|| io::Error::other("pty child reported no pid"))? as libc::pid_t;

    let done = Arc::new(AtomicBool::new(false));
    let claimed = Arc::new(AtomicBool::new(false));
    let watcher = spawn_cancel_watcher(pid, vec![cancel.clone()], claimed, done.clone());

    let master = pair.master;
    let mut reader = master.try_clone_reader().map_err(pty_err)?;

    // Raw mode only when we are actually forwarding a real terminal; the
    // guard restores cooked mode on every exit path, panics included.
    let _raw = if stdin_is_tty && matches!(stdin, StdinSpec::Inherit) {
        RawModeGuard::new(0)
    } else {
        None
    };

    // Stdin plumbing. `master` must stay alive until the pump finishes, so
    // whichever arm does not move it into a thread parks it in `_master_keep`.
    let mut input_threads = Vec::new();
    let mut _master_keep: Option<Box<dyn MasterPty + Send>> = None;
    match stdin {
        StdinSpec::Inherit if stdin_is_tty => {
            let w = dup_master_writer(master.as_ref())?;
            let d = done.clone();
            input_threads.push(thread::spawn(move || {
                forward_stdin_and_resize(w, master, &d);
            }));
        }
        StdinSpec::Bytes(bytes) => {
            let mut w = dup_master_writer(master.as_ref())?;
            _master_keep = Some(master);
            input_threads.push(thread::spawn(move || {
                let _ = w.write_all(&bytes);
                let _ = w.flush();
            }));
        }
        StdinSpec::File(_) => {
            let mut w = dup_master_writer(master.as_ref())?;
            _master_keep = Some(master);
            let mut f = stdin_file.expect("opened above for StdinSpec::File");
            input_threads.push(thread::spawn(move || {
                let _ = io::copy(&mut f, &mut w);
                let _ = w.flush();
            }));
        }
        StdinSpec::Null | StdinSpec::Inherit => {
            // No real tty to forward (tests/CI) or nothing to send: the child
            // still has its pty; just don't feed it.
            _master_keep = Some(master);
        }
    }

    // Output pump: master → tee buffer, plus raw passthrough to the real
    // terminal when there is one.
    let tee: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let pump_done = Arc::new(AtomicBool::new(false));
    let pump = {
        let tee = Arc::clone(&tee);
        let pump_done = Arc::clone(&pump_done);
        let mut passthrough = if stdout_is_tty { dup_stdout() } else { None };
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        tee.lock().expect("tee lock").extend_from_slice(&buf[..n]);
                        if let Some(out) = passthrough.as_mut() {
                            let _ = out.write_all(&buf[..n]);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break, // EIO: slave side fully closed
                }
            }
            pump_done.store(true, Ordering::SeqCst);
        })
    };

    // Reap (zombie-free), then let helpers wind down.
    let raw_status = waitpid_blocking(pid);
    done.store(true, Ordering::SeqCst);

    // Normally EOF lands immediately after exit; give the pump a short grace
    // to drain bytes still buffered in the pty. If a grandchild keeps the
    // slave open we abandon the pump — it exits on its own at EOF — instead
    // of hanging the prompt.
    let deadline = Instant::now() + PUMP_DRAIN_GRACE;
    while !pump_done.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if pump_done.load(Ordering::SeqCst) {
        let _ = pump.join();
    }

    let _ = watcher.join();
    for t in input_threads {
        let _ = t.join();
    }

    let raw_status = raw_status?;
    let (status, signal) = decode_wait_status(raw_status);
    let stdout = mem::take(&mut *tee.lock().expect("tee lock"));
    drop(child); // portable-pty child handle; already reaped via waitpid
    #[allow(clippy::cast_sign_loss)] // pids are positive
    Ok(ExecResult {
        status,
        signal,
        stdout,
        stderr: Vec::new(),
        dur: start.elapsed(),
        pid: pid as u32,
    })
}
