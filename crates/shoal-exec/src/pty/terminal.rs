//! Raw terminal ownership and PTY byte-pump primitives.
//!
//! This module is the only part of the PTY executor that manipulates terminal
//! modes, window sizes, or duplicated file descriptors. The job owner keeps
//! the `MasterPty`; helpers receive owned duplicates so stopping a serve never
//! closes the live PTY.

use std::fs::File;
use std::io::{self, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{MasterPty, PtySize};

/// How often input/output helpers wake to notice teardown.
const INPUT_POLL_MS: i32 = 50;
/// Winsize is re-checked every N input polls (approximately every 200 ms).
const WINSIZE_EVERY_N_POLLS: u32 = 4;

pub(super) type BackgroundOutputSink = Box<dyn FnMut(&[u8]) + Send>;

pub(super) struct OutputPumpConfig {
    pub(super) tee: Arc<Mutex<Vec<u8>>>,
    pub(super) tee_truncated: Arc<AtomicBool>,
    pub(super) passthrough: Option<File>,
    pub(super) background_output: Option<BackgroundOutputSink>,
    pub(super) cap: usize,
    pub(super) serve_done: Arc<AtomicBool>,
    pub(super) pump_done: Arc<AtomicBool>,
}

/// Lock the bounded human-facing tee. If a writer panicked while holding it,
/// its captured prefix is unknowable: discard that prefix, force the result's
/// truncation marker, repair the mutex, and continue capturing subsequent
/// bytes. Raw terminal/background passthrough remains independent.
pub(super) fn lock_tee<'a>(
    tee: &'a Mutex<Vec<u8>>,
    tee_truncated: &AtomicBool,
) -> std::sync::MutexGuard<'a, Vec<u8>> {
    match tee.lock() {
        Ok(bytes) => bytes,
        Err(poisoned) => {
            tee_truncated.store(true, Ordering::SeqCst);
            let mut bytes = poisoned.into_inner();
            bytes.clear();
            tee.clear_poison();
            bytes
        }
    }
}

pub(super) fn pty_err(e: anyhow::Error) -> io::Error {
    // portable-pty wraps operating-system failures in anyhow. Preserve the
    // original io::Error so callers can reliably distinguish ENOENT/E2BIG/etc.
    match e.downcast::<io::Error>() {
        Ok(error) => error,
        Err(error) => io::Error::other(error.to_string()),
    }
}

/// Restores the original termios of `fd` on drop, including during unwind.
pub(super) struct RawModeGuard {
    fd: RawFd,
    orig: libc::termios,
}

impl RawModeGuard {
    /// Put an already-validated terminal `fd` into raw mode.
    pub(super) fn new(fd: RawFd) -> io::Result<Self> {
        // SAFETY: termios syscalls on a caller-owned fd with valid pointers.
        unsafe {
            let mut term = mem::zeroed::<libc::termios>();
            if libc::tcgetattr(fd, &raw mut term) != 0 {
                return Err(io::Error::last_os_error());
            }
            let orig = term;
            libc::cfmakeraw(&raw mut term);
            if libc::tcsetattr(fd, libc::TCSANOW, &raw const term) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd, orig })
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

pub(super) fn initial_pty_size() -> PtySize {
    tty_winsize(0)
        .or_else(|| tty_winsize(1))
        .unwrap_or(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
}

pub(super) fn is_tty(fd: RawFd) -> bool {
    // SAFETY: isatty is a trivial fd query.
    unsafe { libc::isatty(fd) == 1 }
}

/// Push `size` onto the pty represented by an owned duplicate descriptor.
fn set_winsize(fd: RawFd, size: PtySize) {
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.pixel_width,
        ws_ypixel: size.pixel_height,
    };
    // SAFETY: TIOCSWINSZ with a valid winsize pointer on a pty fd we own.
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &raw const ws);
    }
}

/// A dup of fd 1 for raw byte passthrough.
pub(super) fn dup_stdout() -> Option<File> {
    // SAFETY: dup returns a fresh fd we then own; from_raw_fd takes it over.
    let fd = unsafe { libc::dup(1) };
    if fd < 0 {
        None
    } else {
        Some(unsafe { File::from_raw_fd(fd) })
    }
}

/// A side-effect-free duplicate of the pty master descriptor.
pub(super) fn dup_master_fd(master: &dyn MasterPty) -> io::Result<File> {
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

/// Forward real-stdin bytes and propagate terminal resizes until teardown.
pub(super) fn forward_stdin_and_resize(mut writer: File, done: &AtomicBool) {
    let wfd = writer.as_raw_fd();
    let mut buf = [0u8; 4096];
    let mut last = tty_winsize(0);
    let mut ticks: u32 = 0;
    while !done.load(Ordering::SeqCst) {
        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(WINSIZE_EVERY_N_POLLS)
            && let Some(size) = tty_winsize(0)
        {
            let changed = last
                .is_none_or(|previous| previous.rows != size.rows || previous.cols != size.cols);
            if changed {
                set_winsize(wfd, size);
                last = Some(size);
            }
        }
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll on one valid pollfd.
        let ready = unsafe { libc::poll(&raw mut pfd, 1, INPUT_POLL_MS) };
        if ready <= 0 || pfd.revents & (libc::POLLIN | libc::POLLHUP) == 0 {
            continue;
        }
        // SAFETY: read into a valid buffer of the stated length.
        let read = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if read <= 0 {
            break;
        }
        #[allow(clippy::cast_sign_loss)] // read > 0 checked above
        let count = read as usize;
        if writer.write_all(&buf[..count]).is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

/// Drain the pty master into bounded capture and optional presentation sinks.
pub(super) fn pump_output(reader: File, config: OutputPumpConfig) {
    let OutputPumpConfig {
        tee,
        tee_truncated,
        mut passthrough,
        mut background_output,
        cap,
        serve_done,
        pump_done,
    } = config;
    let fd = reader.as_raw_fd();
    let mut buf = [0u8; 8192];
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll on one valid pollfd we own.
        let ready = unsafe { libc::poll(&raw mut pfd, 1, INPUT_POLL_MS) };
        if ready <= 0 {
            if serve_done.load(Ordering::SeqCst) {
                break;
            }
            continue;
        }
        if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
            continue;
        }
        // SAFETY: read into a valid buffer of the stated length.
        let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if read == 0 {
            break;
        }
        if read < 0 {
            match io::Error::last_os_error().raw_os_error() {
                Some(value) if value == libc::EIO => break,
                Some(value) if value == libc::EINTR || value == libc::EAGAIN => continue,
                _ => break,
            }
        }
        #[allow(clippy::cast_sign_loss)] // read > 0 checked above
        let count = read as usize;
        {
            let mut captured = lock_tee(&tee, &tee_truncated);
            if captured.len() < cap {
                let take = (cap - captured.len()).min(count);
                captured.extend_from_slice(&buf[..take]);
                if take < count {
                    tee_truncated.store(true, Ordering::SeqCst);
                }
            } else {
                tee_truncated.store(true, Ordering::SeqCst);
            }
        }
        if let Some(output) = passthrough.as_mut() {
            let _ = output.write_all(&buf[..count]);
        }
        if let Some(output) = background_output.as_mut() {
            output(&buf[..count]);
        }
    }
    pump_done.store(true, Ordering::SeqCst);
}
