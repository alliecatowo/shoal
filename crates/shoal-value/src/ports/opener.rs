//! Desktop opener host adapter and bounded child-lifecycle ownership.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The `open <path>` effect: hand a path to the desktop's default handler.
pub trait Opener: Send + Sync {
    /// Open `path` detached. Unsupported host platforms use
    /// [`io::ErrorKind::Unsupported`], saturated host dispatch uses
    /// [`io::ErrorKind::WouldBlock`], and launcher failures preserve their
    /// original I/O kind for the evaluator's typed error mapping.
    fn open(&self, path: &Path) -> io::Result<()>;
}

/// The default [`Opener`]: detached `xdg-open` on Linux and `open` on macOS,
/// with null stdio. Other hosts fail as unsupported. At most
/// [`MAX_CONCURRENT_OPENERS`] background owners and process groups exist at a
/// time; timeout cleanup terminates the entire owned group and reaps its
/// leader.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdOpener;

const OPENER_REAPER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONCURRENT_OPENERS: usize = 4;
type ChildOwner = Arc<Mutex<Option<std::process::Child>>>;

impl Opener for StdOpener {
    fn open(&self, path: &Path) -> io::Result<()> {
        let mut command = std::process::Command::new(desktop_opener_program(std::env::consts::OS)?);
        command
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        spawn_detached_with(
            opener_supervisor(),
            &mut command,
            OPENER_REAPER_TIMEOUT,
            |owner, pgid, timeout, permit| {
                std::thread::Builder::new()
                    .name("shoal-open-reaper".into())
                    .spawn(move || reap_open_child(&owner, pgid, timeout, permit))
                    .map(|_| ())
            },
        )
        .map(|_| ())
    }
}

fn desktop_opener_program(platform: &str) -> io::Result<&'static str> {
    match platform {
        "linux" => Ok("xdg-open"),
        "macos" => Ok("open"),
        other => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("open: desktop integration is unsupported on {other}"),
        )),
    }
}

#[derive(Debug)]
struct OpenSupervisor {
    max_active: usize,
    active: Mutex<usize>,
}

impl OpenSupervisor {
    const fn new(max_active: usize) -> Self {
        Self {
            max_active,
            active: Mutex::new(0),
        }
    }

    fn acquire(self: &Arc<Self>) -> io::Result<OpenPermit> {
        let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        if *active >= self.max_active {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "open: desktop dispatch capacity ({}) is saturated; retry after an active opener exits",
                    self.max_active
                ),
            ));
        }
        *active += 1;
        Ok(OpenPermit {
            supervisor: self.clone(),
        })
    }

    #[cfg(test)]
    fn active(&self) -> usize {
        *self.active.lock().unwrap_or_else(|p| p.into_inner())
    }
}

struct OpenPermit {
    supervisor: Arc<OpenSupervisor>,
}

impl Drop for OpenPermit {
    fn drop(&mut self) {
        let mut active = self
            .supervisor
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *active = active.saturating_sub(1);
    }
}

fn opener_supervisor() -> &'static Arc<OpenSupervisor> {
    static SUPERVISOR: OnceLock<Arc<OpenSupervisor>> = OnceLock::new();
    SUPERVISOR.get_or_init(|| Arc::new(OpenSupervisor::new(MAX_CONCURRENT_OPENERS)))
}

fn spawn_detached_with(
    supervisor: &Arc<OpenSupervisor>,
    command: &mut std::process::Command,
    timeout: Duration,
    launch_reaper: impl FnOnce(ChildOwner, libc::pid_t, Duration, OpenPermit) -> io::Result<()>,
) -> io::Result<u32> {
    // The launcher is a fresh group leader. This lets timeout cleanup include
    // helper/browser descendants without signalling Shoal itself.
    command.process_group(0);
    let permit = supervisor.acquire()?;
    let child = command
        .spawn()
        .map_err(|error| io::Error::new(error.kind(), format!("open: {error}")))?;
    let pid = child.id();
    let pgid = pid as libc::pid_t;
    let owner = Arc::new(Mutex::new(Some(child)));
    if let Err(error) = launch_reaper(owner.clone(), pgid, timeout, permit) {
        // A failed thread launch drops the moved permit and never ran the
        // closure. Kill/reap the exact group synchronously before reporting.
        if let Some(mut child) = take_child(&owner) {
            kill_group_and_reap(&mut child, pgid);
        }
        return Err(io::Error::new(
            error.kind(),
            format!("open: cannot launch child reaper: {error}"),
        ));
    }
    Ok(pid)
}

fn reap_open_child(owner: &ChildOwner, pgid: libc::pid_t, timeout: Duration, _permit: OpenPermit) {
    let Some(mut child) = take_child(owner) else {
        return;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if start.elapsed() >= timeout => {
                kill_group_and_reap(&mut child, pgid);
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                kill_group_and_reap(&mut child, pgid);
                return;
            }
        }
    }
}

fn take_child(owner: &ChildOwner) -> Option<std::process::Child> {
    match owner.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
}

fn kill_group_and_reap(child: &mut std::process::Child, pgid: libc::pid_t) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    // SAFETY: pgid is the fresh process-group leader created by our Command.
    let group_sent = unsafe { libc::kill(-pgid, libc::SIGKILL) } == 0;
    let leader_sent = child.kill().is_ok();
    if group_sent || leader_sent {
        let _ = child.wait();
    } else {
        // A refused signal must not turn bounded cleanup into an unbounded
        // wait. This final probe still reaps an exit racing the signal.
        let _ = child.try_wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn desktop_program_selection_is_platform_exact_and_fail_closed() {
        assert_eq!(desktop_opener_program("linux").unwrap(), "xdg-open");
        assert_eq!(desktop_opener_program("macos").unwrap(), "open");
        let error = desktop_opener_program("windows").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("windows"));
    }

    fn process_is_gone(pid: u32) -> bool {
        // SAFETY: signal 0 only checks whether the recorded process exists.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    fn wait_until_gone(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !process_is_gone(pid) {
            assert!(Instant::now() < deadline, "opener process {pid} survived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn thread_launcher(
        owner: ChildOwner,
        pgid: libc::pid_t,
        timeout: Duration,
        permit: OpenPermit,
    ) -> io::Result<()> {
        std::thread::Builder::new()
            .spawn(move || reap_open_child(&owner, pgid, timeout, permit))
            .map(|_| ())
    }

    #[test]
    fn quick_exit_is_reaped_and_permit_is_reusable() {
        let supervisor = Arc::new(OpenSupervisor::new(1));
        for _ in 0..2 {
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", "exit 0"]);
            let pid = spawn_detached_with(
                &supervisor,
                &mut command,
                Duration::from_secs(1),
                thread_launcher,
            )
            .expect("spawn quick opener");
            wait_until_gone(pid);
            let deadline = Instant::now() + Duration::from_secs(1);
            while supervisor.active() != 0 {
                assert!(Instant::now() < deadline, "permit was not released");
                std::thread::yield_now();
            }
        }
    }

    #[test]
    fn admission_saturation_is_typed_and_does_not_spawn() {
        let supervisor = Arc::new(OpenSupervisor::new(1));
        let _permit = supervisor.acquire().unwrap();
        let mut command = std::process::Command::new("/definitely/not/a/program");
        let error = spawn_detached_with(
            &supervisor,
            &mut command,
            Duration::from_secs(1),
            thread_launcher,
        )
        .expect_err("capacity must reject before spawn");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("saturated"));
    }

    #[test]
    fn spawn_failure_releases_the_admission_permit() {
        let supervisor = Arc::new(OpenSupervisor::new(1));
        let mut command = std::process::Command::new("/definitely/not/a/program");
        let error = spawn_detached_with(
            &supervisor,
            &mut command,
            Duration::from_secs(1),
            thread_launcher,
        )
        .expect_err("missing opener program must report spawn failure");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(supervisor.active(), 0);
        assert!(supervisor.acquire().is_ok(), "permit must be reusable");
    }

    #[test]
    fn timeout_kills_leader_and_descendant_process_group() {
        let pid_file = std::env::temp_dir().join(format!(
            "shoal-opener-descendant-{}-{}.pid",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let script = format!(
            "sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; wait",
            pid_file.display()
        );
        let supervisor = Arc::new(OpenSupervisor::new(1));
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", &script]);
        let leader = spawn_detached_with(
            &supervisor,
            &mut command,
            Duration::from_millis(100),
            thread_launcher,
        )
        .expect("spawn hung opener tree");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !pid_file.is_file() {
            assert!(
                Instant::now() < deadline,
                "descendant pid was not published"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let descendant = std::fs::read_to_string(&pid_file)
            .unwrap()
            .parse::<u32>()
            .unwrap();
        wait_until_gone(leader);
        wait_until_gone(descendant);
        let _ = std::fs::remove_file(pid_file);
    }

    #[test]
    fn reaper_launch_failure_reaps_group_and_releases_permit() {
        let supervisor = Arc::new(OpenSupervisor::new(1));
        let observed_pid = Arc::new(AtomicU32::new(0));
        let observer = observed_pid.clone();
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let error = spawn_detached_with(
            &supervisor,
            &mut command,
            Duration::from_secs(1),
            move |owner, _, _, _permit| {
                let pid = match owner.lock() {
                    Ok(guard) => guard.as_ref().map(std::process::Child::id),
                    Err(poisoned) => poisoned.into_inner().as_ref().map(std::process::Child::id),
                }
                .expect("launcher observes owned child");
                observer.store(pid, Ordering::SeqCst);
                Err(io::Error::other("injected thread exhaustion"))
            },
        )
        .expect_err("reaper launch must fail closed");
        assert!(error.to_string().contains("child reaper"));
        let pid = observed_pid.load(Ordering::SeqCst);
        assert_ne!(pid, 0);
        assert!(process_is_gone(pid));
        assert_eq!(supervisor.active(), 0);
        assert!(supervisor.acquire().is_ok(), "permit must be reusable");
    }
}
