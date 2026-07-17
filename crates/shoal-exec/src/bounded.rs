//! Small, hostile-input-safe subprocess probes.
//!
//! Unlike the shell's general capture path, probes need a short wall-clock
//! deadline and a deliberately tiny retained-output budget. The child is put
//! in its own process group, both output pipes are drained concurrently, and
//! the whole group is killed and the leader reaped on every timeout or setup
//! failure. As with all Unix process-group containment, a deliberately
//! `setsid`-escaping descendant is outside that boundary.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
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
/// processes or inherited pipe writers.
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
    let stdout = match child.stdout.take() {
        Some(pipe) => pipe,
        None => return setup_failure(&mut child, pgid, "stdout pipe was not created"),
    };
    let stderr = match child.stderr.take() {
        Some(pipe) => pipe,
        None => return setup_failure(&mut child, pgid, "stderr pipe was not created"),
    };

    let retained = Arc::new(Mutex::new(RetainedOutput::default()));
    let stdout_reader = match spawn_reader(stdout, Stream::Stdout, output_cap, retained.clone()) {
        Ok(reader) => reader,
        Err(error) => {
            kill_and_reap(&mut child, pgid);
            return Err(error);
        }
    };
    let stderr_reader = match spawn_reader(stderr, Stream::Stderr, output_cap, retained.clone()) {
        Ok(reader) => reader,
        Err(error) => {
            kill_and_reap(&mut child, pgid);
            let _ = join_reader(stdout_reader);
            return Err(error);
        }
    };

    let (status, timed_out, cleanup_error) = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // The leader may have forked background descendants which
                // still hold the pipes. Terminate the remaining group before
                // joining the readers.
                let cleanup_error = kill_group(pgid).err();
                break (status, false, cleanup_error);
            }
            Ok(None) if start.elapsed() >= timeout => {
                let cleanup_error = kill_group(pgid).err();
                let status = match child.wait() {
                    Ok(status) => status,
                    Err(error) => {
                        let _ = join_reader(stdout_reader);
                        let _ = join_reader(stderr_reader);
                        return Err(error);
                    }
                };
                break (status, true, cleanup_error);
            }
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(error) => {
                kill_and_reap(&mut child, pgid);
                let _ = join_reader(stdout_reader);
                let _ = join_reader(stderr_reader);
                return Err(error);
            }
        }
    };

    let stdout_result = join_reader(stdout_reader);
    let stderr_result = join_reader(stderr_reader);
    if let Some(error) = cleanup_error {
        return Err(error);
    }
    stdout_result?;
    stderr_result?;

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
) -> io::Result<JoinHandle<io::Result<()>>> {
    thread::Builder::new()
        .name(
            match stream {
                Stream::Stdout => "shoal-probe-stdout",
                Stream::Stderr => "shoal-probe-stderr",
            }
            .to_string(),
        )
        .spawn(move || drain_pipe(reader, stream, cap, &retained))
}

fn drain_pipe<R: Read>(
    mut reader: R,
    stream: Stream,
    cap: usize,
    retained: &Mutex<RetainedOutput>,
) -> io::Result<()> {
    let mut chunk = [0_u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => {
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
            Err(error) => return Err(error),
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

fn kill_group(pgid: libc::pid_t) -> io::Result<()> {
    // SAFETY: pgid is the positive pid of a child we placed in a fresh group.
    let result = unsafe { libc::kill(-pgid, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

fn kill_and_reap(child: &mut Child, pgid: libc::pid_t) {
    let _ = kill_group(pgid);
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
}
