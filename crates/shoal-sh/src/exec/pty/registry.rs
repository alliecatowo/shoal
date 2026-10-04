//! Process-global ownership registries for stopped and detached PTY jobs.
//!
//! A detached worker remains the sole owner of its live [`PtyJob`]. The
//! background registry stores only a command sender, allowing the host to ask
//! that worker to retire its pump and transfer ownership back for `fg`.

use std::collections::VecDeque;
use std::io;
use std::sync::Mutex;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::time::Duration;

use super::PtyJob;

/// Parked foreground jobs that stopped under job control.
static PARKED_JOBS: Mutex<VecDeque<PtyJob>> = Mutex::new(VecDeque::new());
/// Command handles for detached workers; workers retain the jobs themselves.
static BACKGROUND_JOBS: Mutex<Vec<(u32, Sender<BackgroundCommand>)>> = Mutex::new(Vec::new());

pub(super) enum BackgroundCommand {
    Foreground(SyncSender<PtyJob>),
    Shutdown,
}

pub(super) fn register_background_job(pid: u32, commands: Sender<BackgroundCommand>) {
    if let Ok(mut jobs) = BACKGROUND_JOBS.lock() {
        jobs.retain(|(registered, _)| *registered != pid);
        jobs.push((pid, commands));
    }
}

pub(super) fn remove_background_job(pid: u32) {
    if let Ok(mut jobs) = BACKGROUND_JOBS.lock() {
        jobs.retain(|(registered, _)| *registered != pid);
    }
}

/// Request ownership of a still-running background PTY. The worker first
/// retires its detached output pump, then moves the live job through this
/// one-shot channel. `None` means the child completed or stopped before the
/// request won the race.
pub fn take_background_job(pid: u32) -> io::Result<Option<PtyJob>> {
    let commands = BACKGROUND_JOBS.lock().ok().and_then(|jobs| {
        jobs.iter()
            .find(|(registered, _)| *registered == pid)
            .map(|(_, commands)| commands.clone())
    });
    let Some(commands) = commands else {
        return Ok(None);
    };
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if commands
        .send(BackgroundCommand::Foreground(reply_tx))
        .is_err()
    {
        return Ok(None);
    }
    match reply_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(job) => Ok(Some(job)),
        Err(mpsc::RecvTimeoutError::Disconnected) => Ok(None),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "background PTY ownership transfer timed out",
        )),
    }
}

/// Park a stopped job so the host can later resume it by pid.
pub(super) fn park_job(job: PtyJob) {
    if let Ok(mut jobs) = PARKED_JOBS.lock() {
        jobs.push_back(job);
    }
}

/// Remove and return the parked (stopped) PTY job for `pid`, if any.
#[must_use]
pub fn take_stopped_job(pid: u32) -> Option<PtyJob> {
    let mut jobs = PARKED_JOBS.lock().ok()?;
    let index = jobs.iter().position(|job| job.pid() == pid)?;
    jobs.remove(index)
}

/// Kill and reap every running or stopped PTY owned by the shell.
pub fn shutdown_stopped_jobs() {
    if let Ok(mut jobs) = BACKGROUND_JOBS.lock() {
        for (_, commands) in jobs.drain(..) {
            let _ = commands.send(BackgroundCommand::Shutdown);
        }
    }
    if let Ok(mut jobs) = PARKED_JOBS.lock() {
        jobs.clear();
    }
}
