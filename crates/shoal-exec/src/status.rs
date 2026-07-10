//! Wait-status decoding: `WIFEXITED` / `WIFSIGNALED` via libc, and signal
//! names per TDD §13.6 (never the shell-style `128+n` encoding).

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
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: plain waitpid on a pid we spawned; the status pointer is a
        // valid, live stack slot.
        let r = unsafe { libc::waitpid(pid, &raw mut status, 0) };
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
