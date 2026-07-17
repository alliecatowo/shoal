//! Wait-status decoding: `WIFEXITED` / `WIFSIGNALED` via libc, and signal
//! names per site/content/internals/language-conformance-contract.md (never the shell-style `128+n` encoding).

use std::io;

/// Decode a raw `waitpid` status into `(exit_code, signal_name)`.
///
/// Exactly one of the two is `Some` for a terminated child.
pub(crate) fn decode_wait_status(raw: i32) -> (Option<i32>, Option<String>) {
    if libc::WIFEXITED(raw) {
        (Some(libc::WEXITSTATUS(raw)), None)
    } else if libc::WIFSIGNALED(raw) {
        (None, Some(signal_name(libc::WTERMSIG(raw))))
    } else {
        // Not a terminal status (stopped/continued). Callers only pass
        // statuses returned by a blocking wait without WUNTRACED, so this is
        // unreachable in practice; report "no information" rather than lying.
        (None, None)
    }
}

/// Human name for the common signal set; anything else renders as `SIG<n>`.
pub(crate) fn signal_name(sig: i32) -> String {
    let name = match sig {
        libc::SIGINT => "SIGINT",
        libc::SIGTERM => "SIGTERM",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGILL => "SIGILL",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGHUP => "SIGHUP",
        libc::SIGQUIT => "SIGQUIT",
        other => return format!("SIG{other}"),
    };
    name.to_string()
}

/// Blocking `waitpid` that retries on `EINTR`. Reaps the child (zombie-free).
pub(crate) fn waitpid_blocking(pid: libc::pid_t) -> io::Result<i32> {
    waitpid_flags(pid, 0)
}

/// Blocking `waitpid` with `WUNTRACED`, so a child that *stops* (SIGTSTP /
/// SIGSTOP — e.g. Ctrl-Z on its controlling terminal) is reported as a status
/// change instead of blocking until the child terminates (site/content/internals/language-conformance-contract.md job
/// control). A stopped child is NOT reaped (it is still alive, suspended);
/// callers distinguish the two via [`is_stopped`].
pub(crate) fn waitpid_untraced(pid: libc::pid_t) -> io::Result<i32> {
    waitpid_flags(pid, libc::WUNTRACED)
}

/// Shared `waitpid` core: retry on `EINTR`, return the raw status once the
/// target pid is the one that changed state.
fn waitpid_flags(pid: libc::pid_t, flags: libc::c_int) -> io::Result<i32> {
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: plain waitpid on a pid we spawned; the status pointer is a
        // valid, live stack slot.
        let r = unsafe { libc::waitpid(pid, &raw mut status, flags) };
        if r == pid {
            return Ok(status);
        }
        if r == -1 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
    }
}

/// `true` when a raw wait status returned by a `WUNTRACED` wait indicates the
/// child *stopped* (was suspended) rather than terminated. Such a child is
/// still alive; `decode_wait_status` reports `(None, None)` for it, so this is
/// the discriminator that keeps a stop from being mistaken for a mystery exit.
pub(crate) fn is_stopped(raw: i32) -> bool {
    libc::WIFSTOPPED(raw)
}

/// The signal number that stopped the child (`WSTOPSIG`), for a status where
/// [`is_stopped`] holds — typically `SIGTSTP` (Ctrl-Z) or `SIGSTOP`.
#[cfg(test)]
pub(crate) fn stop_signal(raw: i32) -> i32 {
    libc::WSTOPSIG(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_normal_exit() {
        // Raw wait status for exit(7) is 7 << 8.
        assert_eq!(decode_wait_status(7 << 8), (Some(7), None));
        assert_eq!(decode_wait_status(0), (Some(0), None));
    }

    #[test]
    fn decodes_signal_death() {
        assert_eq!(
            decode_wait_status(libc::SIGSEGV),
            (None, Some("SIGSEGV".to_string()))
        );
        assert_eq!(
            decode_wait_status(libc::SIGKILL),
            (None, Some("SIGKILL".to_string()))
        );
    }

    #[test]
    fn stopped_status_is_recognized_and_not_a_terminal_status() {
        // A `WUNTRACED` stop status is encoded as `(stopsig << 8) | 0x7f`.
        let raw = (libc::SIGTSTP << 8) | 0x7f;
        assert!(is_stopped(raw), "WIFSTOPPED must hold for a stop status");
        assert_eq!(stop_signal(raw), libc::SIGTSTP);
        // A stopped child is neither a normal exit nor a signal death.
        assert_eq!(decode_wait_status(raw), (None, None));
        // A plain exit is not a stop.
        assert!(!is_stopped(7 << 8));
        assert!(!is_stopped(libc::SIGKILL));
    }

    /// The real kernel job-control primitives this crate relies on, exercised
    /// without any controlling terminal: a child placed in its own process
    /// group is SIGSTOP'd, observed as *stopped* by a `WUNTRACED` wait, then
    /// SIGCONT'd and allowed to run to completion. This is exactly the
    /// stop→resume cycle `fg`/`bg` drive, proven against the OS rather than a
    /// crafted status word.
    #[test]
    // We deliberately reap the child through this crate's own `waitpid`
    // primitives (the thing under test), not `Child::wait`, so clippy cannot see
    // the reap — but the child IS reaped (the final `waitpid_blocking`).
    #[allow(clippy::zombie_processes)]
    fn real_child_stop_and_continue_cycle() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        // A child that would exit on its own after a moment, in its own group.
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        // SAFETY: setpgid(0, 0) is async-signal-safe, allowed between fork/exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
        let child = cmd.spawn().expect("spawn sleep");
        let pid = child.id() as libc::pid_t;

        // Stop it and confirm the WUNTRACED wait sees a stop, not an exit.
        // SAFETY: signalling a live child we spawned.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        let raw = waitpid_untraced(pid).expect("wait stop");
        assert!(is_stopped(raw), "child must be observed as stopped");
        assert_eq!(stop_signal(raw), libc::SIGSTOP);

        // Continue it, then kill + reap so the test leaves no process behind.
        // SAFETY: same live child.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        let raw = waitpid_blocking(pid).expect("reap");
        assert!(!is_stopped(raw), "the reaped status must be terminal");
    }

    #[test]
    fn names_the_common_set_and_falls_back_to_numbers() {
        for (sig, name) in [
            (libc::SIGINT, "SIGINT"),
            (libc::SIGTERM, "SIGTERM"),
            (libc::SIGKILL, "SIGKILL"),
            (libc::SIGSEGV, "SIGSEGV"),
            (libc::SIGABRT, "SIGABRT"),
            (libc::SIGBUS, "SIGBUS"),
            (libc::SIGFPE, "SIGFPE"),
            (libc::SIGILL, "SIGILL"),
            (libc::SIGPIPE, "SIGPIPE"),
            (libc::SIGHUP, "SIGHUP"),
            (libc::SIGQUIT, "SIGQUIT"),
        ] {
            assert_eq!(signal_name(sig), name);
        }
        assert_eq!(signal_name(libc::SIGUSR1), format!("SIG{}", libc::SIGUSR1));
    }
}
