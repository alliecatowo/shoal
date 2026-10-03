//! One foreground/background service stint for a live PTY job.
//!
//! A stint owns all temporary helper threads. The [`PtyJob`] retains the PTY
//! master and child across stints so a stop can be parked and resumed without
//! closing the slave or losing output.

use std::fs::File;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::cancel::CancelToken;
use crate::status::{is_stopped, waitpid_untraced, waitpid_untraced_nohang};
use crate::watcher::spawn_cancel_watcher;

use super::PtyJob;
use super::registry::BackgroundCommand;
use super::terminal::{
    BackgroundOutputSink, OutputPumpConfig, RawModeGuard, dup_master_fd, dup_stdout,
    forward_stdin_and_resize, pump_output,
};

/// After the child is reaped, wait briefly for the pump to consume PTY EOF.
const PUMP_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Non-tty stdin consumed exactly once, on the job's first service stint.
pub(super) enum Feed {
    Bytes(Vec<u8>),
    File(File),
}

/// The ownership/status transition produced by one service stint.
pub(super) enum Wait {
    Exited(i32),
    Stopped,
    Foreground(SyncSender<PtyJob>),
    Shutdown,
}

pub(super) struct ServeOptions<'a> {
    foreground: bool,
    resume: bool,
    background_commands: Option<&'a Receiver<BackgroundCommand>>,
    background_output: Option<BackgroundOutputSink>,
}

impl ServeOptions<'_> {
    pub(super) fn foreground(resume: bool) -> Self {
        Self {
            foreground: true,
            resume,
            background_commands: None,
            background_output: None,
        }
    }
}

impl<'a> ServeOptions<'a> {
    pub(super) fn background(
        commands: &'a Receiver<BackgroundCommand>,
        output: Option<BackgroundOutputSink>,
    ) -> Self {
        Self {
            foreground: false,
            resume: true,
            background_commands: Some(commands),
            background_output: output,
        }
    }
}

/// Attach the input, output, and cancellation helpers; wait for a transition;
/// then retire every helper owned by this stint.
pub(super) fn serve(
    job: &mut PtyJob,
    cancel: &CancelToken,
    options: ServeOptions<'_>,
) -> io::Result<Wait> {
    let ServeOptions {
        foreground,
        resume,
        background_commands,
        background_output,
    } = options;

    // Raw terminal ownership is scoped to this foreground stint and restores
    // the original termios on every return or unwind path.
    let _raw = if foreground && job.forward_tty {
        Some(RawModeGuard::new(0)?)
    } else {
        None
    };

    let serve_done = Arc::new(AtomicBool::new(false));
    let helpers = attach_input(job, foreground, &serve_done)?;
    let (pump, pump_done) =
        attach_output(job, foreground, background_output, Arc::clone(&serve_done))?;

    let claimed = Arc::new(AtomicBool::new(false));
    let watcher = spawn_cancel_watcher(
        job.pgid,
        vec![cancel.clone()],
        claimed,
        Arc::clone(&serve_done),
    );

    // Attach helpers before SIGCONT so output emitted immediately after resume
    // cannot fall into a gap. Preserve wait/signal errors until cleanup ends.
    let waited = if resume {
        signal_cont(job.pgid).and_then(|()| wait_for_transition(job.pid, background_commands))
    } else {
        wait_for_transition(job.pid, background_commands)
    };

    retire_helpers(serve_done, pump_done, pump, watcher, helpers);

    match waited? {
        Wait::Exited(raw) if is_stopped(raw) => Ok(Wait::Stopped),
        transition => Ok(transition),
    }
}

fn attach_input(
    job: &mut PtyJob,
    foreground: bool,
    serve_done: &Arc<AtomicBool>,
) -> io::Result<Vec<thread::JoinHandle<()>>> {
    let mut helpers = Vec::new();
    if let Some(feed) = job.pending_feed.take() {
        let mut writer = dup_master_fd(job.master.as_ref())?;
        helpers.push(thread::spawn(move || match feed {
            Feed::Bytes(bytes) => {
                let _ = writer.write_all(&bytes);
                let _ = writer.flush();
            }
            Feed::File(mut file) => {
                let _ = io::copy(&mut file, &mut writer);
                let _ = writer.flush();
            }
        }));
    } else if foreground && job.forward_tty {
        let writer = dup_master_fd(job.master.as_ref())?;
        let done = Arc::clone(serve_done);
        helpers.push(thread::spawn(move || {
            forward_stdin_and_resize(writer, &done);
        }));
    }
    Ok(helpers)
}

fn attach_output(
    job: &PtyJob,
    foreground: bool,
    background_output: Option<BackgroundOutputSink>,
    serve_done: Arc<AtomicBool>,
) -> io::Result<(thread::JoinHandle<()>, Arc<AtomicBool>)> {
    let reader = dup_master_fd(job.master.as_ref())?;
    let pump_done = Arc::new(AtomicBool::new(false));
    let passthrough = if job.stdout_is_tty && (foreground || background_output.is_none()) {
        dup_stdout()
    } else {
        None
    };
    let config = OutputPumpConfig {
        tee: Arc::clone(&job.tee),
        tee_truncated: Arc::clone(&job.tee_truncated),
        passthrough,
        background_output,
        cap: job.cap,
        serve_done,
        pump_done: Arc::clone(&pump_done),
    };
    let pump = thread::spawn(move || pump_output(reader, config));
    Ok((pump, pump_done))
}

fn wait_for_transition(
    pid: libc::pid_t,
    commands: Option<&Receiver<BackgroundCommand>>,
) -> io::Result<Wait> {
    let Some(commands) = commands else {
        return waitpid_untraced(pid).map(Wait::Exited);
    };
    loop {
        if let Some(raw) = waitpid_untraced_nohang(pid)? {
            return Ok(Wait::Exited(raw));
        }
        match commands.try_recv() {
            Ok(BackgroundCommand::Foreground(reply)) => return Ok(Wait::Foreground(reply)),
            Ok(BackgroundCommand::Shutdown) => return Ok(Wait::Shutdown),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn signal_cont(pgid: libc::pid_t) -> io::Result<()> {
    // SAFETY: signalling a process group is memory-safe.
    if unsafe { libc::kill(-pgid, libc::SIGCONT) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn retire_helpers(
    serve_done: Arc<AtomicBool>,
    pump_done: Arc<AtomicBool>,
    pump: thread::JoinHandle<()>,
    watcher: thread::JoinHandle<()>,
    helpers: Vec<thread::JoinHandle<()>>,
) {
    serve_done.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + PUMP_DRAIN_GRACE;
    while !pump_done.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if pump_done.load(Ordering::SeqCst) {
        let _ = pump.join();
    }
    // A grandchild may keep the slave open and flood it past the grace period.
    // In that case the detached pump exits on its next idle poll instead of
    // holding the prompt hostage, preserving the established cleanup policy.
    let _ = watcher.join();
    for helper in helpers {
        let _ = helper.join();
    }
}
