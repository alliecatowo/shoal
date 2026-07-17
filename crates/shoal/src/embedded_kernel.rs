//! Private one-shot kernel transport for the interactive shell.

use shoal_mcp::{Config, KernelClient, LocalAuthMode};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const EMBEDDED_FD: i32 = 3;

pub(crate) struct EmbeddedKernelConfig {
    pub(crate) session: String,
    pub(crate) state_dir: PathBuf,
    pub(crate) socket: PathBuf,
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
        .arg("--socket")
        .arg(&config.socket)
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
    let guard = EmbeddedKernelChild { child, shutdown };
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
