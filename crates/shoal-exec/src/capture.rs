//! Capture mode: piped stdout/stderr, no controlling tty, child in its own
//! process group, both pipes drained concurrently (deadlock-free).

use std::io::{self, Read, Write};
use std::mem;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::cancel::CancelToken;
use crate::status::decode_wait_status;
use crate::watcher::spawn_cancel_watcher;
use crate::which::resolve_program;
use crate::{ExecMode, ExecResult, ExecSpec, StdinSpec};

/// A capture-mode child spawned for streaming consumption (background tasks,
/// `tail -f`, …). The caller drains [`StreamingChild::stdout`] /
/// [`StreamingChild::stderr`] and then calls [`StreamingChild::wait`].
///
/// Dropping a `StreamingChild` without waiting SIGKILLs its process group and
/// reaps it — the crate never leaves zombies behind.
pub struct StreamingChild {
    /// The child's process id (also its process-group id).
    pub pid: u32,
    /// The child's stdout pipe. Read to EOF for the full stream.
    pub stdout: Box<dyn Read + Send>,
    /// The child's stderr pipe. Read to EOF for the full stream.
    pub stderr: Box<dyn Read + Send>,
    inner: StreamInner,
}

impl StreamingChild {
    /// Wait for exit, honoring the same INT → TERM → KILL cancellation
    /// escalation as [`crate::run`] (both the token given at spawn time and
    /// the one given here are watched).
    ///
    /// `stdout`/`stderr` in the result are empty — the caller drained the
    /// readers. Any reader still held by this struct is dropped before
    /// waiting, so an undrained child blocked on a full pipe gets `EPIPE`
    /// rather than deadlocking the wait.
    ///
    /// # Errors
    ///
    /// Propagates OS errors from `waitpid`; these do not occur in normal
    /// operation. The child is reaped either way.
    pub fn wait(self, cancel: &CancelToken) -> io::Result<ExecResult> {
        let StreamingChild {
            stdout,
            stderr,
            mut inner,
            ..
        } = self;
        drop(stdout);
        drop(stderr);
        inner.wait_reap(cancel)
    }
}

/// Owns the OS child plus its helper threads; reaps on drop if needed.
struct StreamInner {
    child: Child,
    pgid: libc::pid_t,
    start: Instant,
    done: Arc<AtomicBool>,
    claimed: Arc<AtomicBool>,
    spawn_token: CancelToken,
    threads: Vec<JoinHandle<()>>,
    reaped: bool,
}

impl StreamInner {
    fn wait_reap(&mut self, cancel: &CancelToken) -> io::Result<ExecResult> {
        if !self.spawn_token.same(cancel) {
            self.threads.push(spawn_cancel_watcher(
                self.pgid,
                vec![cancel.clone()],
                self.claimed.clone(),
                self.done.clone(),
            ));
        }
        let status = self.child.wait();
        self.reaped = status.is_ok();
        self.done.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let status = status?;
        let (code, signal) = decode_wait_status(status.into_raw());
        Ok(ExecResult {
            status: code,
            signal,
            stdout: Vec::new(),
            stderr: Vec::new(),
            dur: self.start.elapsed(),
            pid: self.child.id(),
        })
    }
}

impl Drop for StreamInner {
    fn drop(&mut self) {
        if !self.reaped {
            // Dropped without wait(): kill the whole group and reap so the
            // crate never leaks zombies or runaway process trees.
            // SAFETY: signalling a process group is memory-safe.
            unsafe {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
            let _ = self.child.wait();
        }
        self.done.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Spawn `spec` (which must be [`crate::ExecMode::Capture`]-shaped) with
/// piped stdout/stderr for streaming consumption.
///
/// The cancellation watcher starts immediately: tripping `cancel` while the
/// caller is still draining the pipes interrupts the child (INT → TERM →
/// KILL against its process group), which unblocks the reader at EOF.
///
/// # Errors
///
/// Program resolution failures ([`io::ErrorKind::NotFound`]), stdin-file open
/// failures, and spawn errors (including `E2BIG`) surface here.
pub fn spawn_capture(spec: ExecSpec, cancel: &CancelToken) -> io::Result<StreamingChild> {
    if spec.mode != ExecMode::Capture {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "spawn_capture requires ExecMode::Capture",
        ));
    }
    let program = resolve_program(&spec.argv, &spec.env)?;
    let mut cmd = Command::new(&program);
    cmd.args(&spec.argv[1..]);
    cmd.current_dir(&spec.cwd);
    cmd.env_clear();
    cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut stdin_bytes = None;
    match spec.stdin {
        StdinSpec::Null => {
            cmd.stdin(Stdio::null());
        }
        StdinSpec::Inherit => {
            cmd.stdin(Stdio::inherit());
        }
        StdinSpec::Bytes(b) => {
            cmd.stdin(Stdio::piped());
            stdin_bytes = Some(b);
        }
        StdinSpec::File(p) => {
            cmd.stdin(Stdio::from(std::fs::File::open(&p)?));
        }
    }

    // Child gets its own process group so cancellation can signal the whole
    // tree without touching the shell itself.
    // SAFETY: setpgid is async-signal-safe and allowed between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }

    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let pgid = pid as libc::pid_t;

    let mut threads = Vec::new();
    if let Some(bytes) = stdin_bytes {
        let mut sink = child.stdin.take().expect("stdin was configured as piped");
        threads.push(thread::spawn(move || {
            // EPIPE (child exited before reading everything) is expected.
            let _ = sink.write_all(&bytes);
        }));
    }

    let done = Arc::new(AtomicBool::new(false));
    let claimed = Arc::new(AtomicBool::new(false));
    threads.push(spawn_cancel_watcher(
        pgid,
        vec![cancel.clone()],
        claimed.clone(),
        done.clone(),
    ));

    let stdout = child.stdout.take().expect("stdout was configured as piped");
    let stderr = child.stderr.take().expect("stderr was configured as piped");
    Ok(StreamingChild {
        pid,
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        inner: StreamInner {
            child,
            pgid,
            start,
            done,
            claimed,
            spawn_token: cancel.clone(),
            threads,
            reaped: false,
        },
    })
}

/// Blocking capture run: drains stdout and stderr on two threads (so a child
/// filling both pipes can never deadlock), waits, and returns the collected
/// bytes.
pub(crate) fn run_capture(spec: ExecSpec, cancel: &CancelToken) -> io::Result<ExecResult> {
    let mut child = spawn_capture(spec, cancel)?;
    let out = mem::replace(&mut child.stdout, Box::new(io::empty()));
    let err = mem::replace(&mut child.stderr, Box::new(io::empty()));
    let t_out = thread::spawn(move || drain(out));
    let t_err = thread::spawn(move || drain(err));
    let mut res = child.wait(cancel)?;
    res.stdout = t_out.join().unwrap_or_default();
    res.stderr = t_err.join().unwrap_or_default();
    Ok(res)
}

fn drain(mut r: Box<dyn Read + Send>) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf);
    buf
}
