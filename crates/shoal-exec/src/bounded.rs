//! Small, hostile-input-safe subprocess probes.
//!
//! Unlike the shell's general capture path, probes need a short wall-clock
//! deadline and a deliberately tiny retained-output budget. The child is put
//! in its own process group, both output pipes are drained concurrently, and
//! the whole group is killed and the leader reaped on every timeout or setup
//! failure. As with all Unix process-group containment, a deliberately
//! `setsid`-escaping descendant is outside that boundary.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Result of a subprocess probe whose wall time and retained output were
/// bounded by [`run_bounded_command`].
#[derive(Debug)]
pub struct BoundedCommandOutput {
    /// Termination status of the process-group leader. On timeout this is
    /// normally the status produced by `SIGKILL`.
    pub status: ExitStatus,
    /// Retained stdout prefix. The combined stdout + stderr length never
    /// exceeds the caller's output cap.
    pub stdout: Vec<u8>,
    /// Retained stderr prefix. See [`BoundedCommandOutput::stdout`].
    pub stderr: Vec<u8>,
    /// Whether any output was discarded after the combined cap was reached.
    pub truncated: bool,
    /// Whether the deadline expired before the process-group leader exited.
    pub timed_out: bool,
    /// Process-group id assigned to the child. Exposed for diagnostics and
    /// cleanup assertions; the group has been terminated before return.
    pub pgid: u32,
    /// Elapsed wall time including process cleanup and pipe draining.
    pub duration: Duration,
}

#[derive(Default)]
struct RetainedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Run a short-lived helper command with a hard wall-clock deadline and a
/// combined retained-output cap.
///
/// The command's stdin/stdout/stderr configuration is replaced with null and
/// two pipes. The child becomes a fresh process-group leader. Both pipes are
/// drained concurrently even after the cap is reached, preventing a writer
/// from defeating the deadline by filling a pipe. On timeout, reader-launch
/// failure, or wait failure, the entire process group is killed and the leader
/// is reaped before this function returns.
///
/// A leader that exits successfully but leaves descendants behind is also
/// followed by a group kill. This keeps probes from leaking background helper
/// processes or inherited pipe writers. A descendant that deliberately leaves
/// the group (for example with `setsid`) cannot safely be signalled through
/// group ownership; nonblocking, explicitly stoppable readers ensure such a
/// process still cannot hold this function hostage through an inherited pipe.
///
/// # Errors
///
/// Returns spawn, thread-launch, pipe-read, signal, or wait errors after first
/// making a best effort to kill the process group and reap its leader.
pub fn run_bounded_command(
    command: &mut Command,
    timeout: Duration,
    output_cap: usize,
) -> io::Result<BoundedCommandOutput> {
    run_bounded_command_inner(command, timeout, output_cap, None)
}

pub(crate) fn run_bounded_command_cancellable(
    command: &mut Command,
    timeout: Duration,
    output_cap: usize,
    cancel: &crate::CancelToken,
) -> io::Result<BoundedCommandOutput> {
    run_bounded_command_inner(command, timeout, output_cap, Some(cancel))
}

fn run_bounded_command_inner(
    command: &mut Command,
    timeout: Duration,
    output_cap: usize,
    cancel: Option<&crate::CancelToken>,
) -> io::Result<BoundedCommandOutput> {
    if cancel.is_some_and(crate::CancelToken::is_cancelled) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "bounded command cancelled before spawn",
        ));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setpgid is async-signal-safe and is permitted between fork and
    // exec. It gives this probe a group we can terminate without affecting the
    // shell or unrelated processes.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }

    let start = Instant::now();
    let mut child = command.spawn()?;
    let pgid = child.id() as libc::pid_t;
    let _process_group = cancel.map(|cancel| cancel.register_process_group(pgid));
    let stdout = match child.stdout.take() {
        Some(pipe) => pipe,
        None => return setup_failure(&mut child, pgid, "stdout pipe was not created"),
    };
    let stderr = match child.stderr.take() {
        Some(pipe) => pipe,
        None => return setup_failure(&mut child, pgid, "stderr pipe was not created"),
    };
    if let Err(error) = set_nonblocking(&stdout).and_then(|()| set_nonblocking(&stderr)) {
        kill_and_reap(&mut child, pgid);
        return Err(error);
    }

    let retained = Arc::new(Mutex::new(RetainedOutput::default()));
    let stop_readers = Arc::new(AtomicBool::new(false));
    let stdout_reader = match spawn_reader(
        stdout,
        Stream::Stdout,
        output_cap,
        retained.clone(),
        stop_readers.clone(),
    ) {
        Ok(reader) => reader,
        Err(error) => {
            stop_readers.store(true, Ordering::Release);
            kill_and_reap(&mut child, pgid);
            return Err(error);
        }
    };
    let stderr_reader = match spawn_reader(
        stderr,
        Stream::Stderr,
        output_cap,
        retained.clone(),
        stop_readers.clone(),
    ) {
        Ok(reader) => reader,
        Err(error) => {
            stop_readers.store(true, Ordering::Release);
            kill_and_reap(&mut child, pgid);
            let _ = join_reader(stdout_reader);
            return Err(error);
        }
    };

    let (status, timed_out, cancelled, cleanup_error) = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // The leader may have forked background descendants which
                // still hold the pipes. Terminate the remaining group before
                // joining the readers.
                let cleanup_error = kill_group(pgid).err();
                break (status, false, false, cleanup_error);
            }
            Ok(None)
                if cancel.is_some_and(crate::CancelToken::is_cancelled)
                    || start.elapsed() >= timeout =>
            {
                let cancelled = cancel.is_some_and(crate::CancelToken::is_cancelled);
                stop_readers.store(true, Ordering::Release);
                let (signalled, cleanup_error) = signal_owned_processes(&mut child, pgid);
                if !signalled {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            break (status, !cancelled, cancelled, cleanup_error);
                        }
                        Ok(None) => {
                            let _ = join_reader(stdout_reader);
                            let _ = join_reader(stderr_reader);
                            return Err(cleanup_error.unwrap_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::PermissionDenied,
                                    "could not terminate timed-out bounded command",
                                )
                            }));
                        }
                        Err(error) => {
                            let _ = join_reader(stdout_reader);
                            let _ = join_reader(stderr_reader);
                            return Err(error);
                        }
                    }
                }
                let status = match child.wait() {
                    Ok(status) => status,
                    Err(error) => {
                        let _ = join_reader(stdout_reader);
                        let _ = join_reader(stderr_reader);
                        return Err(error);
                    }
                };
                break (status, !cancelled, cancelled, cleanup_error);
            }
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(error) => {
                stop_readers.store(true, Ordering::Release);
                kill_and_reap(&mut child, pgid);
                let _ = join_reader(stdout_reader);
                let _ = join_reader(stderr_reader);
                return Err(error);
            }
        }
    };

    stop_readers.store(true, Ordering::Release);
    let stdout_result = join_reader(stdout_reader);
    let stderr_result = join_reader(stderr_reader);
    if let Some(error) = cleanup_error {
        return Err(error);
    }
    stdout_result?;
    stderr_result?;
    if cancelled {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "bounded command cancelled",
        ));
    }

    let retained = take_retained(retained);
    Ok(BoundedCommandOutput {
        status,
        stdout: retained.stdout,
        stderr: retained.stderr,
        truncated: retained.truncated,
        timed_out,
        #[allow(clippy::cast_sign_loss)]
        pgid: pgid as u32,
        duration: start.elapsed(),
    })
}

fn setup_failure<T>(child: &mut Child, pgid: libc::pid_t, msg: &'static str) -> io::Result<T> {
    kill_and_reap(child, pgid);
    Err(io::Error::other(msg))
}

fn spawn_reader<R: Read + Send + 'static>(
    reader: R,
    stream: Stream,
    cap: usize,
    retained: Arc<Mutex<RetainedOutput>>,
    stop: Arc<AtomicBool>,
) -> io::Result<JoinHandle<io::Result<()>>> {
    thread::Builder::new()
        .name(
            match stream {
                Stream::Stdout => "shoal-probe-stdout",
                Stream::Stderr => "shoal-probe-stderr",
            }
            .to_string(),
        )
        .spawn(move || drain_pipe(reader, stream, cap, &retained, &stop))
}

fn drain_pipe<R: Read>(
    mut reader: R,
    stream: Stream,
    cap: usize,
    retained: &Mutex<RetainedOutput>,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut chunk = [0_u8; 8192];
    // Once the command is done, retain the already-buffered prefix and make a
    // bounded effort to empty kernel pipe buffers. A continuously writing
    // escaped descendant must not turn cleanup back into an unbounded drain.
    let mut post_stop_budget: usize = 64 * 1024;
    loop {
        let stopping = stop.load(Ordering::Acquire);
        if stopping && post_stop_budget == 0 {
            return Ok(());
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => {
                if stopping {
                    post_stop_budget = post_stop_budget.saturating_sub(read);
                }
                let mut output = match retained.lock() {
                    Ok(output) => output,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let used = output.stdout.len().saturating_add(output.stderr.len());
                let keep = read.min(cap.saturating_sub(used));
                let destination = match stream {
                    Stream::Stdout => &mut output.stdout,
                    Stream::Stderr => &mut output.stderr,
                };
                destination.extend_from_slice(&chunk[..keep]);
                output.truncated |= keep < read;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if stopping {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error),
        }
    }
}

fn set_nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
    let raw_fd = fd.as_raw_fd();
    let flags = loop {
        // SAFETY: F_GETFL only reads flags for a live pipe descriptor.
        let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFL) };
        if flags != -1 {
            break flags;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    };
    loop {
        // SAFETY: F_SETFL mutates flags for the same live descriptor; keeping
        // its existing flags and adding O_NONBLOCK is valid for a pipe.
        if unsafe { libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } != -1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

fn join_reader(reader: JoinHandle<io::Result<()>>) -> io::Result<()> {
    match reader.join() {
        Ok(result) => result,
        Err(_) => Err(io::Error::other("bounded-command pipe reader panicked")),
    }
}

fn take_retained(retained: Arc<Mutex<RetainedOutput>>) -> RetainedOutput {
    match Arc::try_unwrap(retained) {
        Ok(mutex) => match mutex.into_inner() {
            Ok(output) => output,
            Err(poisoned) => poisoned.into_inner(),
        },
        Err(shared) => {
            let mut guard = match shared.lock() {
                Ok(output) => output,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::take(&mut *guard)
        }
    }
}

/// `true` means SIGKILL was delivered; `false` means the group no longer
/// existed by the time it was signalled.
fn kill_group(pgid: libc::pid_t) -> io::Result<bool> {
    // SAFETY: pgid is the positive pid of a child we placed in a fresh group.
    let result = unsafe { libc::kill(-pgid, libc::SIGKILL) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error)
    }
}

fn kill_and_reap(child: &mut Child, pgid: libc::pid_t) {
    let (signalled, _) = signal_owned_processes(child, pgid);
    if signalled {
        let _ = child.wait();
    } else {
        // Never turn cleanup into an unbounded wait when the OS refused both
        // kill routes. A final nonblocking probe still reaps an already-exited
        // child in the common race.
        let _ = child.try_wait();
    }
}

/// `true` means SIGKILL was delivered; `false` means the leader had already
/// exited by the time it was signalled.
fn kill_leader(child: &mut Child) -> io::Result<bool> {
    match child.kill() {
        Ok(()) => Ok(true),
        Err(error)
            if error.kind() == io::ErrorKind::InvalidInput
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn signal_owned_processes(child: &mut Child, pgid: libc::pid_t) -> (bool, Option<io::Error>) {
    let group = kill_group(pgid);
    let leader = kill_leader(child);
    let signalled = matches!(group, Ok(true)) || matches!(leader, Ok(true));
    let error = group.err().or_else(|| leader.err());
    (signalled, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn bounded_spec(argv: Vec<std::ffi::OsString>, cwd: std::path::PathBuf) -> crate::ExecSpec {
        crate::ExecSpec {
            argv,
            cwd,
            env: Vec::new(),
            stdin: crate::StdinSpec::Null,
            mode: crate::ExecMode::Capture,
            sandbox: None,
            spill: None,
        }
    }

    #[test]
    fn bounded_exec_spec_honors_explicit_environment_and_cwd() {
        let directory = tempfile::tempdir().expect("tempdir");
        let spec = bounded_spec(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s:%s' \"$REEF_SENTINEL\" \"$PWD\"".into(),
            ],
            directory.path().to_path_buf(),
        );
        let mut spec = spec;
        spec.env.push(("REEF_SENTINEL".into(), "bounded".into()));
        let output = crate::run_bounded(
            spec,
            Duration::from_secs(1),
            1024,
            &crate::CancelToken::new(),
        )
        .expect("bounded spec");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("bounded:{}", directory.path().display())
        );
    }

    #[test]
    fn pre_cancelled_bounded_exec_spec_never_spawns() {
        let directory = tempfile::tempdir().expect("tempdir");
        let marker = directory.path().join("spawned");
        let spec = bounded_spec(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("touch '{}'", marker.display()).into(),
            ],
            directory.path().to_path_buf(),
        );
        let cancel = crate::CancelToken::new();
        cancel.cancel();
        let error = crate::run_bounded(spec, Duration::from_secs(1), 1024, &cancel)
            .expect_err("cancelled bounded command");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!marker.exists());
    }

    #[test]
    fn drains_both_pipes_while_retaining_only_combined_cap() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "i=0; while [ $i -lt 20000 ]; do printf 0123456789; printf abcdefghij >&2; i=$((i+1)); done",
        ]);
        let output = run_bounded_command(&mut command, Duration::from_secs(2), 4096)
            .expect("bounded flood probe");

        assert!(output.status.success());
        assert!(!output.timed_out);
        assert!(output.truncated);
        assert_eq!(output.stdout.len() + output.stderr.len(), 4096);
        assert!(output.duration < Duration::from_secs(2));
    }

    #[test]
    fn timeout_kills_and_reaps_the_whole_process_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let child_pid_path = dir.path().join("descendant.pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", child_pid_path.display());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]);

        let output = run_bounded_command(&mut command, Duration::from_millis(80), 1024)
            .expect("timed probe");
        assert!(output.timed_out);
        assert!(output.duration < Duration::from_secs(1));

        let descendant: libc::pid_t = fs::read_to_string(child_pid_path)
            .expect("descendant pid")
            .trim()
            .parse()
            .expect("numeric pid");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY: signal 0 only checks whether this recorded pid exists.
            let result = unsafe { libc::kill(descendant, 0) };
            if result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "descendant {descendant} survived"
            );
            thread::sleep(Duration::from_millis(10));
        }

        // SAFETY: signal 0 only checks group existence. No member of the
        // probe's dedicated process group may remain after return.
        let group = unsafe { libc::kill(-(output.pgid as libc::pid_t), 0) };
        assert_eq!(group, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn successful_leader_cannot_leave_inherited_pipe_descendant_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let child_pid_path = dir.path().join("descendant.pid");
        let script = format!(
            "sleep 30 & echo $! > '{}'; exit 0",
            child_pid_path.display()
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]);

        let output = run_bounded_command(&mut command, Duration::from_secs(2), 1024)
            .expect("successful probe");
        assert!(output.status.success());
        assert!(!output.timed_out);
        assert!(output.duration < Duration::from_secs(1));

        let descendant: libc::pid_t = fs::read_to_string(child_pid_path)
            .expect("descendant pid")
            .trim()
            .parse()
            .expect("numeric pid");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY: signal 0 only checks whether this recorded pid exists.
            let result = unsafe { libc::kill(descendant, 0) };
            if result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "descendant {descendant} survived"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn escaped_pipe_writer_cannot_hold_reader_joins_hostage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let escaped_pid_path = dir.path().join("escaped.pid");
        let script = format!(
            "/usr/bin/setsid /bin/sh -c 'echo $$ > \"{}\"; while :; do printf x; done' & while [ ! -s '{}' ]; do :; done; exit 0",
            escaped_pid_path.display(),
            escaped_pid_path.display()
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]);

        let output = run_bounded_command(&mut command, Duration::from_secs(2), 1024)
            .expect("escaped-writer probe");
        assert!(output.status.success());
        assert!(!output.timed_out);
        assert!(output.duration < Duration::from_secs(1));

        let escaped: libc::pid_t = fs::read_to_string(escaped_pid_path)
            .expect("escaped pid")
            .trim()
            .parse()
            .expect("numeric pid");
        // The escaped process is outside the group ownership boundary. Close
        // the adversarial fixture explicitly if dropping its inherited pipe
        // did not already terminate it with SIGPIPE.
        // SAFETY: this exact pid was written by the test fixture moments ago.
        let _ = unsafe { libc::kill(escaped, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY: signal 0 only checks whether this recorded pid exists.
            let result = unsafe { libc::kill(escaped, 0) };
            if result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "escaped writer {escaped} survived"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
