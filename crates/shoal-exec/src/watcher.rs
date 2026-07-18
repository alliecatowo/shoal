//! Cancellation watcher: polls tokens and escalates INT → TERM → KILL
//! against the child's process group (site/content/internals/language-conformance-contract.md).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::cancel::CancelToken;

/// How often watcher threads poll their flags.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Grace period between escalation rungs (SIGINT → SIGTERM → SIGKILL).
pub(crate) const ESCALATION_GRACE: Duration = Duration::from_secs(3);

/// Spawn a thread that waits for any of `tokens` to trip, then walks the
/// signal ladder against the process *group* `pgid` (i.e. `kill(-pgid, …)`).
///
/// `done` is set by the owner once the child has been reaped; the watcher
/// checks it at every step and exits promptly. `claimed` ensures that when
/// several watchers guard the same child (e.g. distinct tokens given to
/// `spawn_capture` and `wait`), only one of them runs the escalation.
pub(crate) fn spawn_cancel_watcher(
    pgid: libc::pid_t,
    tokens: Vec<CancelToken>,
    claimed: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        loop {
            if done.load(Ordering::SeqCst) {
                return;
            }
            if tokens.iter().any(CancelToken::is_cancelled) {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
        if claimed.swap(true, Ordering::SeqCst) {
            return; // another watcher owns the escalation
        }
        if tokens.iter().any(CancelToken::has_suspended_processes) {
            // Normally `CancelToken::cancel` has already resumed the registry.
            // This per-child fallback also unsticks cancellation if the
            // advisory multi-group registry had to reject a poisoned access.
            unsafe {
                libc::kill(-pgid, libc::SIGCONT);
            }
        }
        let ladder = [
            (libc::SIGINT, Some(ESCALATION_GRACE)),
            (libc::SIGTERM, Some(ESCALATION_GRACE)),
            (libc::SIGKILL, None),
        ];
        for (sig, grace) in ladder {
            // Checking `done` immediately before signalling narrows (but, as
            // in every shell, cannot fully close) the reaped-pid-reuse race.
            if done.load(Ordering::SeqCst) {
                return;
            }
            // SAFETY: sending a signal to a process group is memory-safe.
            unsafe {
                libc::kill(-pgid, sig);
            }
            let Some(grace) = grace else { return };
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(POLL_INTERVAL);
            }
        }
    })
}
