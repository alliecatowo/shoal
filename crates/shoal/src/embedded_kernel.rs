//! Private one-shot kernel transport for the interactive shell.

use shoal_mcp::{Config, KernelClient, LocalAuthMode};
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const EMBEDDED_FD: i32 = 3;
const EMBEDDED_READY_PROTOCOL: u64 = 1;
const EMBEDDED_READY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct EmbeddedKernelConfig {
    pub(crate) session: String,
    pub(crate) state_dir: PathBuf,
    pub(crate) policy: Option<PathBuf>,
    pub(crate) program: Option<PathBuf>,
}

pub(crate) struct EmbeddedKernelChild {
    child: Child,
    shutdown: UnixStream,
}

impl EmbeddedKernelChild {
    fn terminate(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        // Closing either clone shuts down this endpoint and wakes the child's
        // private request loop. The child then stops its public accept loop,
        // drops the journal/WAL cleanly, and removes the socket path.
        let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
        let graceful_deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < graceful_deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid = self.child.id() as i32;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

impl Drop for EmbeddedKernelChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

pub(crate) fn connect(
    config: EmbeddedKernelConfig,
) -> Result<(KernelClient, EmbeddedKernelChild), String> {
    let (parent, child_end) = UnixStream::pair().map_err(|error| error.to_string())?;
    let shutdown = parent.try_clone().map_err(|error| error.to_string())?;
    let child_fd = child_end.as_raw_fd();
    let program = config.program.unwrap_or_else(kernel_program);
    let mut command = Command::new(&program);
    command
        .arg("--embedded-fd")
        .arg(EMBEDDED_FD.to_string())
        .arg("--state-dir")
        .arg(&config.state_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(policy) = &config.policy {
        command.arg("--policy").arg(policy);
    }
    // SAFETY: this closure uses only async-signal-safe descriptor syscalls
    // between fork and exec. The kernel remains in the foreground process
    // group; its embedded-mode caught SIGINT handler keeps it alive while
    // command exec restores default signal behavior for PTY children.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(child_fd, EMBEDDED_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(EMBEDDED_FD, libc::F_GETFD);
            if flags == -1
                || libc::fcntl(EMBEDDED_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|error| {
        format!(
            "cannot start embedded kernel {}: {error}",
            program.display()
        )
    })?;
    drop(child_end);
    let mut guard = EmbeddedKernelChild { child, shutdown };
    wait_until_ready(&parent, &mut guard)?;
    let client_config = Config {
        socket: PathBuf::new(),
        session: Some(config.session),
        token: None,
        local_auth: LocalAuthMode::LocalHuman,
    };
    let client = KernelClient::from_stream(parent, &client_config, "shoal-repl", true)
        .map_err(|error| error.to_string())?;
    Ok((client, guard))
}

fn wait_until_ready(stream: &UnixStream, guard: &mut EmbeddedKernelChild) -> Result<(), String> {
    let reader_stream = stream.try_clone().map_err(|error| error.to_string())?;
    reader_stream
        .set_read_timeout(Some(EMBEDDED_READY_TIMEOUT))
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(reader_stream);
    let mut line = String::new();
    let read = reader.read_line(&mut line).map_err(|error| {
        format!(
            "embedded kernel did not become ready within {}s: {error}{}",
            EMBEDDED_READY_TIMEOUT.as_secs(),
            child_status_suffix(&mut guard.child)
        )
    })?;
    if read == 0 {
        return Err(format!(
            "embedded kernel closed its private transport before readiness{}",
            child_status_suffix(&mut guard.child)
        ));
    }
    let frame: serde_json::Value = serde_json::from_str(line.trim_end())
        .map_err(|error| format!("embedded kernel sent an invalid readiness frame: {error}"))?;
    if frame["shoal_embedded"]["ready"] != true
        || frame["shoal_embedded"]["protocol"] != EMBEDDED_READY_PROTOCOL
    {
        return Err(format!(
            "embedded kernel readiness protocol mismatch: expected version {EMBEDDED_READY_PROTOCOL}"
        ));
    }
    // `set_read_timeout` is a socket option shared by descriptor clones.
    // Restore ordinary blocking RPC behavior before handing the endpoint to
    // KernelClient.
    stream
        .set_read_timeout(None)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn child_status_suffix(child: &mut Child) -> String {
    match child.try_wait() {
        Ok(Some(status)) => format!(" (child exited with {status})"),
        Ok(None) => String::new(),
        Err(error) => format!(" (child status unavailable: {error})"),
    }
}

fn kernel_program() -> PathBuf {
    if let Some(program) = std::env::var_os("SHOAL_KERNEL_BIN") {
        return program.into();
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(parent) = current.parent()
    {
        let sibling = parent.join("shoal-kernel");
        if sibling.is_file() {
            return sibling;
        }
    }
    Path::new("shoal-kernel").to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_child_exit_is_reported_before_rpc_client_construction() {
        let temp = tempfile::tempdir().unwrap();
        let result = connect(EmbeddedKernelConfig {
            session: "early-exit".into(),
            state_dir: temp.path().join("state"),
            policy: None,
            // `/bin/false` is not present on macOS runners. Both supported
            // Unix targets provide the utility at this canonical path, which
            // lets the test exercise a successfully spawned child that exits
            // before readiness rather than the unrelated spawn-error path.
            program: Some(PathBuf::from("/usr/bin/false")),
        });
        let error = match result {
            Ok(_) => panic!("a child that never sends readiness must not connect"),
            Err(error) => error,
        };
        assert!(
            error.contains("before readiness") || error.contains("did not become ready"),
            "unexpected startup error: {error}"
        );
    }
}
