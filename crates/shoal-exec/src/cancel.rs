//! Arc-based cancellation token shared between the caller and watcher threads.

use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct ProcessGroups {
    active: Mutex<HashSet<libc::pid_t>>,
    suspended: AtomicBool,
}

/// Removes one live process group from its cancellation epoch when the owning
/// child has been reaped or transferred out of that execution path.
#[derive(Debug)]
pub(crate) struct ProcessGroupLease {
    groups: Arc<ProcessGroups>,
    pgid: libc::pid_t,
}

impl Drop for ProcessGroupLease {
    fn drop(&mut self) {
        let mut active = match self.groups.active.lock() {
            Ok(active) => active,
            Err(poisoned) => {
                let mut active = poisoned.into_inner();
                active.clear();
                self.groups.active.clear_poison();
                return;
            }
        };
        active.remove(&self.pgid);
    }
}

/// A cheaply-clonable cancellation flag.
///
/// All clones share one flag: cancelling any clone cancels them all. Passing
/// a clone of the same token to [`crate::run`] / [`crate::spawn_capture`] and
/// later to [`StreamingChild::wait`](crate::StreamingChild::wait) is the
/// expected pattern; distinct tokens are also honored (either one cancels).
#[derive(Debug)]
struct CancelInner {
    own: Arc<AtomicBool>,
    checks: Vec<Arc<AtomicBool>>,
    groups: Arc<ProcessGroups>,
    resume_groups_on_cancel: bool,
}

/// A token owns one cancellation flag and may additionally observe the flags
/// of a parent token. Clones share the exact same cancellation identity.
#[derive(Debug, Clone)]
pub struct CancelToken(Arc<CancelInner>);

impl Default for CancelToken {
    fn default() -> Self {
        let own = Arc::new(AtomicBool::new(false));
        Self(Arc::new(CancelInner {
            own: own.clone(),
            checks: vec![own],
            groups: Arc::new(ProcessGroups::default()),
            resume_groups_on_cancel: true,
        }))
    }
}

impl CancelToken {
    /// Create a fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a fresh independently-cancellable child that also observes
    /// cancellation of `parent`. Cancelling the child never cancels the parent.
    #[must_use]
    pub fn linked(parent: &CancelToken) -> Self {
        let own = Arc::new(AtomicBool::new(false));
        let mut checks = Vec::with_capacity(parent.0.checks.len() + 1);
        checks.push(own.clone());
        checks.extend(parent.0.checks.iter().cloned());
        Self(Arc::new(CancelInner {
            own,
            checks,
            groups: parent.0.groups.clone(),
            resume_groups_on_cancel: false,
        }))
    }

    /// Trip the flag. Idempotent; never blocks.
    pub fn cancel(&self) {
        if self.0.resume_groups_on_cancel && self.0.groups.suspended.load(Ordering::Acquire) {
            let _ = self.resume_processes();
        }
        self.0.own.store(true, Ordering::SeqCst);
    }

    /// Has [`CancelToken::cancel`] been called on this token (or any clone)?
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.checks.iter().any(|flag| flag.load(Ordering::SeqCst))
    }

    /// Suspend every currently active child process group owned by this epoch.
    /// Returns the number of live groups stopped. A zero result means the
    /// current work is evaluator-only (or between child processes) and cannot
    /// honestly be suspended by the process-control backend.
    pub fn suspend_processes(&self) -> io::Result<usize> {
        self.signal_processes(libc::SIGSTOP, libc::SIGCONT)
    }

    /// Resume every currently suspended child process group owned by this
    /// epoch. Returns the number of live groups continued.
    pub fn resume_processes(&self) -> io::Result<usize> {
        self.signal_processes(libc::SIGCONT, libc::SIGSTOP)
    }

    pub(crate) fn register_process_group(&self, pgid: libc::pid_t) -> ProcessGroupLease {
        let mut active = match self.0.groups.active.lock() {
            Ok(active) => active,
            Err(poisoned) => {
                let mut active = poisoned.into_inner();
                // This registry is advisory process-control state. After an
                // unwind its old membership is uncertain, so discard it rather
                // than risking a signal to a later pid reuse.
                active.clear();
                self.0.groups.active.clear_poison();
                active
            }
        };
        active.insert(pgid);
        if self.0.groups.suspended.load(Ordering::Acquire) {
            // SAFETY: a linked child registered while its task is suspended
            // must join that state before it can run ahead of its siblings.
            unsafe {
                libc::kill(-pgid, libc::SIGSTOP);
            }
        }
        ProcessGroupLease {
            groups: self.0.groups.clone(),
            pgid,
        }
    }

    fn signal_processes(&self, signal: libc::c_int, rollback: libc::c_int) -> io::Result<usize> {
        let mut active = match self.0.groups.active.lock() {
            Ok(active) => active,
            Err(poisoned) => {
                let mut active = poisoned.into_inner();
                active.clear();
                self.0.groups.active.clear_poison();
                return Err(io::Error::other(
                    "task process-control registry was reconstructed after poison",
                ));
            }
        };
        let mut signalled = Vec::new();
        let mut gone = Vec::new();
        for &pgid in active.iter() {
            // SAFETY: signalling an owned process group is memory-safe. The
            // lease remains registered until its child is reaped, preventing
            // pid reuse while this lock is held.
            if unsafe { libc::kill(-pgid, signal) } == 0 {
                signalled.push(pgid);
                continue;
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                gone.push(pgid);
                continue;
            }
            for pgid in signalled {
                // Best-effort rollback keeps a partial multi-process control
                // failure from being reported as a coherent state change.
                unsafe {
                    libc::kill(-pgid, rollback);
                }
            }
            return Err(error);
        }
        for pgid in gone {
            active.remove(&pgid);
        }
        if !signalled.is_empty() {
            self.0
                .groups
                .suspended
                .store(signal == libc::SIGSTOP, Ordering::Release);
        }
        Ok(signalled.len())
    }

    /// Do `self` and `other` share the same underlying flag?
    pub(crate) fn same(&self, other: &CancelToken) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub(crate) fn has_suspended_processes(&self) -> bool {
        self.0.groups.suspended.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_token_observes_parent_without_cancelling_it() {
        let parent = CancelToken::new();
        let child = CancelToken::linked(&parent);
        child.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());

        let parent = CancelToken::new();
        let child = CancelToken::linked(&parent);
        parent.cancel();
        assert!(child.is_cancelled());
    }

    #[test]
    fn linked_tokens_share_process_control_membership() {
        let parent = CancelToken::new();
        let child = CancelToken::linked(&parent);
        let lease = child.register_process_group(i32::MAX);
        // No such process group exists, so it is pruned rather than counted.
        assert_eq!(parent.suspend_processes().unwrap(), 0);
        drop(lease);
    }
}
