//! PtyTee mode: the child runs on a real PTY as session leader; output
//! streams raw to the real terminal and is teed into the result buffer.
//!
//! Job control (site/content/internals/language-conformance-contract.md) is layered on top of this without disturbing the
//! passthrough. Every PtyTee child is a session/process-group leader (portable-
//! pty calls `setsid` in the child, so its process-group id equals its pid — we
//! never `setpgid` from the parent, which would `EPERM` on a session leader).
//! A **foreground** child is waited on with `WUNTRACED`, so a Ctrl-Z (the pty
//! line discipline turns the forwarded `^Z` byte into `SIGTSTP` for the child's
//! foreground process group) surfaces as a *stop* instead of hanging the shell.
//! On a stop, the still-live PTY (master + child) is packaged into a [`PtyJob`]
//! and parked so the host can resume it in the foreground (`fg`) or background
//! (`bg`). A child that simply runs to completion behaves byte-for-byte as it
//! did before job control existed.
//!
//! **Terminal handoff.** In this PTY-tee model the child owns *its own* pty as
//! session leader; the shell owns the *real* terminal. They are on different
//! terminals, so the classic `tcsetpgrp(real_tty, child_pgid)` dance does not
//! apply (the child is not in the real terminal's session — such a call would
//! `EPERM`/`ENOTTY`). The effective "give the child the terminal" step here is
//! engaging raw mode on the real terminal and forwarding its input to the pty;
//! "reclaim the terminal" is restoring cooked mode (the [`RawModeGuard`] drop).
//! The shell must additionally ignore `SIGTTOU`/`SIGTTIN`/`SIGTSTP` so those
//! terminal-control operations never suspend the shell itself — that is the
//! host's job and the REPL installs those dispositions (see `crates/shoal/
//! src/repl.rs`); children still get the default disposition because `exec`
//! resets caught signals to `SIG_DFL`, which is exactly why Ctrl-Z can stop
//! them.

use std::fs::File;
use std::io;
use std::mem;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use portable_pty::{Child, CommandBuilder, MasterPty, native_pty_system};
use shoal_leash::EnforcementStatus;

use crate::cancel::CancelToken;
use crate::status::{decode_wait_status, waitpid_blocking};
use crate::which::resolve_program;
use crate::{ExecResult, ExecSpec, StdinSpec};

mod registry;
mod service;
mod terminal;

use registry::{park_job, register_background_job, remove_background_job};
pub use registry::{shutdown_stopped_jobs, take_background_job, take_stopped_job};
use service::{Feed, ServeOptions, Wait, serve};
use terminal::{BackgroundOutputSink, initial_pty_size, is_tty, lock_tee, pty_err};

/// A PTY foreground command and everything needed to keep it alive across a
/// stop and later resume it (site/content/internals/language-conformance-contract.md). Created for every PtyTee run; held by
/// the stopped-job registry only when the child stops rather than finishes.
///
/// The `master` is retained (never moved into a helper thread) precisely so the
/// child's pty slave stays open while it is stopped — dropping the master would
/// `SIGHUP` the child on resume. On drop without a clean reap, the whole group
/// is continued-then-killed so no stopped child is orphaned.
pub struct PtyJob {
    master: Box<dyn MasterPty + Send>,
    /// Kept for ownership/lifetime; the child is reaped via our own `waitpid`
    /// (`std::process::Child`'s drop neither waits nor kills), never `.wait()`.
    _child: Box<dyn Child + Send + Sync>,
    pid: libc::pid_t,
    pgid: libc::pid_t,
    tee: Arc<Mutex<Vec<u8>>>,
    tee_truncated: Arc<AtomicBool>,
    /// Whether a real interactive terminal is being forwarded to the child.
    forward_tty: bool,
    /// One-shot non-tty stdin, consumed on the first serve.
    pending_feed: Option<Feed>,
    start: Instant,
    enforcement: Option<EnforcementStatus>,
    cap: usize,
    display: String,
    stdout_is_tty: bool,
    /// `true` once our own `waitpid` has reaped the child, so `Drop`/`kill_and_
    /// reap` know not to signal a dead (possibly pid-reused) process.
    reaped: bool,
}

impl PtyJob {
    /// The child's pid (also its process-group id).
    #[must_use]
    #[allow(clippy::cast_sign_loss)] // pids are positive
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    /// The child's process-group id (`kill(-pgid, …)` reaches the whole job).
    #[must_use]
    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// The command's display form, for a jobs listing.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.display
    }

    /// Build the terminal-exit result and mark the child reaped.
    fn exit_result(&mut self, raw: i32) -> ExecResult {
        let (status, signal) = decode_wait_status(raw);
        let stdout = mem::take(&mut *lock_tee(&self.tee, &self.tee_truncated));
        let truncated = self.tee_truncated.load(Ordering::SeqCst);
        self.reaped = true;
        #[allow(clippy::cast_sign_loss)] // pids are positive
        ExecResult {
            status,
            signal,
            stdout,
            stderr: Vec::new(),
            truncated,
            // PtyTee (statement position) never spills to disk: its bytes
            // already reached the real terminal and the tee is a bounded
            // convenience — site/content/internals/process-execution.md value-position spill is a Capture-mode concern.
            stdout_spill: None,
            dur: self.start.elapsed(),
            pid: self.pid as u32,
            pgid: self.pgid as u32,
            stopped: false,
            enforcement: self.enforcement.take(),
        }
    }

    /// Build the *stopped* result (child alive, suspended). Snapshots the tee
    /// (clone, not take) so a later resume keeps appending to the same buffer.
    fn stopped_result(&self) -> ExecResult {
        let stdout = lock_tee(&self.tee, &self.tee_truncated).clone();
        let truncated = self.tee_truncated.load(Ordering::SeqCst);
        #[allow(clippy::cast_sign_loss)] // pids are positive
        ExecResult {
            status: None,
            signal: None,
            stdout,
            stderr: Vec::new(),
            truncated,
            // A stopped PtyTee job is not a value-position capture; no spill.
            stdout_spill: None,
            dur: self.start.elapsed(),
            pid: self.pid as u32,
            pgid: self.pgid as u32,
            stopped: true,
            enforcement: self.enforcement.clone(),
        }
    }

    /// Continue-then-kill the whole group and reap, for an abandoned job.
    fn kill_and_reap(&mut self) {
        if self.reaped {
            return;
        }
        // SAFETY: signalling a process group is memory-safe. SIGCONT first so a
        // stopped child can actually act on the kill.
        unsafe {
            libc::kill(-self.pgid, libc::SIGCONT);
            libc::kill(-self.pgid, libc::SIGKILL);
        }
        let _ = waitpid_blocking(self.pid);
        self.reaped = true;
    }

    /// Resume this job in the **foreground** (`fg`): `SIGCONT`, re-attach the
    /// terminal, and wait again with `WUNTRACED`. Returns the terminal result
    /// when the child finishes; if it stops again, it is re-parked and the
    /// returned [`ExecResult`] has `stopped: true`.
    ///
    /// # Errors
    /// Propagates a `waitpid`/pty-plumbing [`io::Error`].
    pub fn resume_foreground(mut self, cancel: &CancelToken) -> io::Result<ExecResult> {
        match serve(&mut self, cancel, ServeOptions::foreground(true))? {
            Wait::Exited(raw) => Ok(self.exit_result(raw)),
            Wait::Stopped => {
                let res = self.stopped_result();
                park_job(self);
                Ok(res)
            }
            Wait::Foreground(_) | Wait::Shutdown => unreachable!("no background control channel"),
        }
    }

    /// Attach the foreground terminal to a job already running in the
    /// background. Unlike [`PtyJob::resume_foreground`], this does not send
    /// SIGCONT: ownership transfer did not stop the process group.
    pub fn foreground_running(mut self, cancel: &CancelToken) -> io::Result<ExecResult> {
        match serve(&mut self, cancel, ServeOptions::foreground(false))? {
            Wait::Exited(raw) => Ok(self.exit_result(raw)),
            Wait::Stopped => {
                let result = self.stopped_result();
                park_job(self);
                Ok(result)
            }
            Wait::Foreground(_) | Wait::Shutdown => unreachable!("no background control channel"),
        }
    }

    /// Resume this job in the **background** (`bg`): `SIGCONT` and let it run
    /// detached. A background pump keeps teeing the child's output to the real
    /// terminal and reaps it when it finally exits; stdin is not forwarded (a
    /// background job does not own the terminal input). A background job is not
    /// cancelled by the foreground Ctrl-C, so it runs under its own fresh,
    /// never-tripped [`CancelToken`].
    pub fn resume_background(self) {
        self.resume_background_notify(|_| {});
    }

    /// Background resume with one terminal notification. The callback runs
    /// after the child has either exited, stopped again and been re-parked, or
    /// failed during PTY service. Hosts use this to reconcile their separate
    /// job table without sharing evaluator state with the worker thread.
    pub fn resume_background_notify<F>(self, notify: F)
    where
        F: FnOnce(io::Result<ExecResult>) + Send + 'static,
    {
        self.resume_background_notify_inner(None, notify);
    }

    /// Background resume whose output is delivered to a host-owned bounded
    /// presentation path instead of being written concurrently to fd 1. The
    /// callback runs on the PTY pump thread and must remain nonblocking.
    pub fn resume_background_notify_with_output<F, O>(self, output: O, notify: F)
    where
        F: FnOnce(io::Result<ExecResult>) + Send + 'static,
        O: FnMut(&[u8]) + Send + 'static,
    {
        self.resume_background_notify_inner(Some(Box::new(output)), notify);
    }

    fn resume_background_notify_inner<F>(mut self, output: Option<BackgroundOutputSink>, notify: F)
    where
        F: FnOnce(io::Result<ExecResult>) + Send + 'static,
    {
        let pid = self.pid();
        let (commands_tx, commands_rx) = mpsc::channel();
        register_background_job(pid, commands_tx);
        thread::spawn(move || {
            let cancel = CancelToken::new();
            match serve(
                &mut self,
                &cancel,
                ServeOptions::background(&commands_rx, output),
            ) {
                Ok(Wait::Exited(raw)) => {
                    remove_background_job(pid);
                    notify(Ok(self.exit_result(raw)));
                }
                // Stopped again in the background (unusual): re-park so a later
                // `fg` can still find it.
                Ok(Wait::Stopped) => {
                    remove_background_job(pid);
                    let result = self.stopped_result();
                    park_job(self);
                    notify(Ok(result));
                }
                Ok(Wait::Foreground(reply)) => {
                    remove_background_job(pid);
                    let _ = reply.send(self);
                }
                Ok(Wait::Shutdown) => {
                    remove_background_job(pid);
                    self.kill_and_reap();
                }
                Err(error) => {
                    remove_background_job(pid);
                    self.kill_and_reap();
                    notify(Err(error));
                }
            }
        });
    }
}

impl Drop for PtyJob {
    fn drop(&mut self) {
        // A job dropped without a clean reap — e.g. a parked job abandoned when
        // the shell exits — must not leave a stopped child orphaned.
        self.kill_and_reap();
    }
}

/// Run `spec` on a real PTY, teeing the merged output stream. In interactive
/// foreground use the child may be *stopped* (Ctrl-Z) instead of finishing, in
/// which case the returned [`ExecResult`] has `stopped: true` and the live PTY
/// is parked for resumption (see the module docs and [`take_stopped_job`]).
pub(crate) fn run_pty(mut spec: ExecSpec, cancel: &CancelToken) -> io::Result<ExecResult> {
    // A PTY master has no portable write-half-close. Injecting the terminal's
    // canonical VEOF byte is not an EOF in raw/noncanonical child modes and
    // may instead corrupt the input stream. Incremental finite stdin therefore
    // requires Capture's real pipe; reject it before sandboxing or spawning.
    if matches!(&spec.stdin, StdinSpec::Stream(_)) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "incremental stdin requires Capture mode; a PTY cannot half-close input",
        ));
    }
    let enforcement = crate::sandbox::apply(&mut spec)?;
    let ExecSpec {
        argv,
        cwd,
        env,
        stdin,
        ..
    } = spec;
    let program = resolve_program(&argv, &env, &cwd)?;

    // portable-pty's Unix fork helper currently aborts in the child when its
    // exec-error report itself cannot be written after E2BIG. Reject Linux's
    // fixed per-string limit up front so E2BIG remains an ordinary io error.
    #[cfg(target_os = "linux")]
    if argv.iter().any(|arg| arg.as_bytes().len() >= 131_072)
        || env
            .iter()
            .any(|(key, value)| key.as_bytes().len() + value.as_bytes().len() + 1 >= 131_072)
    {
        return Err(io::Error::from_raw_os_error(libc::E2BIG));
    }

    let stdin_is_tty = is_tty(0);
    let stdout_is_tty = is_tty(1);

    // Open a File stdin source before spawning so errors surface early.
    let stdin_file = match &stdin {
        StdinSpec::File(p) => Some(File::open(p)?),
        _ => None,
    };

    let size = initial_pty_size();
    let pty = native_pty_system();
    let pair = pty.openpty(size).map_err(pty_err)?;

    let mut cmd = CommandBuilder::new(&program);
    for a in &argv[1..] {
        cmd.arg(a);
    }
    cmd.cwd(&cwd);
    cmd.env_clear();
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let display = argv
        .iter()
        .map(|x| x.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");

    let start = Instant::now();
    // portable-pty makes the child a session leader with the slave as its
    // controlling tty; E2BIG and friends surface here as io errors.
    let child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
    drop(pair.slave); // parent must not hold the slave or EOF never arrives
    let pid = child
        .process_id()
        .ok_or_else(|| io::Error::other("pty child reported no pid"))? as libc::pid_t;
    // setsid (done by portable-pty in the child) makes the child its own
    // session AND process-group leader, so its pgid is its pid. We deliberately
    // do NOT setpgid from the parent: a session leader cannot be moved to
    // another group (EPERM), and setsid already gives the isolated group job
    // control needs.
    let pgid = pid;

    let forward_tty = stdin_is_tty && matches!(stdin, StdinSpec::Inherit);
    let pending_feed = match stdin {
        StdinSpec::Bytes(bytes) => Some(Feed::Bytes(bytes)),
        StdinSpec::File(_) => Some(Feed::File(
            stdin_file.expect("opened above for StdinSpec::File"),
        )),
        StdinSpec::Stream(_) => unreachable!("stream stdin rejected before PTY spawn"),
        StdinSpec::Null | StdinSpec::Inherit => None,
    };

    let mut job = PtyJob {
        master: pair.master,
        _child: child,
        pid,
        pgid,
        tee: Arc::new(Mutex::new(Vec::new())),
        tee_truncated: Arc::new(AtomicBool::new(false)),
        forward_tty,
        pending_feed,
        start,
        enforcement,
        cap: crate::capture_hard_cap(),
        display,
        stdout_is_tty,
        reaped: false,
    };

    match serve(&mut job, cancel, ServeOptions::foreground(false)) {
        Ok(Wait::Exited(raw)) => Ok(job.exit_result(raw)),
        Ok(Wait::Stopped) => {
            let res = job.stopped_result();
            park_job(job);
            Ok(res)
        }
        Ok(Wait::Foreground(_) | Wait::Shutdown) => unreachable!("no background control channel"),
        Err(e) => {
            job.kill_and_reap();
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExecMode;
    use std::ffi::OsString;
    use std::io::{Read as _, Write as _};
    use std::os::fd::{FromRawFd as _, IntoRawFd as _};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use super::terminal::{OutputPumpConfig, RawModeGuard, pump_output};

    fn stream_file(stream: UnixStream) -> File {
        // SAFETY: `into_raw_fd` transfers ownership and `File` takes it over.
        unsafe { File::from_raw_fd(stream.into_raw_fd()) }
    }

    #[test]
    fn poisoned_tee_discards_uncertain_prefix_but_preserves_raw_passthrough() {
        let tee = Arc::new(Mutex::new(b"uncertain-prefix".to_vec()));
        let poisoner = Arc::clone(&tee);
        assert!(
            thread::spawn(move || {
                let _bytes = poisoner.lock().expect("inject tee poison");
                panic!("inject tee poison");
            })
            .join()
            .is_err()
        );

        let truncated = Arc::new(AtomicBool::new(false));
        let serve_done = Arc::new(AtomicBool::new(false));
        let pump_done = Arc::new(AtomicBool::new(false));
        let (reader, mut writer) = UnixStream::pair().expect("input pipe");
        let (mut passthrough_reader, passthrough) = UnixStream::pair().expect("passthrough pipe");
        let pump = {
            let tee = Arc::clone(&tee);
            let truncated = Arc::clone(&truncated);
            let serve_done = Arc::clone(&serve_done);
            let pump_done = Arc::clone(&pump_done);
            thread::spawn(move || {
                pump_output(
                    stream_file(reader),
                    OutputPumpConfig {
                        tee,
                        tee_truncated: truncated,
                        passthrough: Some(stream_file(passthrough)),
                        background_output: None,
                        cap: 1024,
                        serve_done,
                        pump_done,
                    },
                );
            })
        };
        writer.write_all(b"certain-output").expect("write input");
        drop(writer);
        pump.join().expect("pump contains tee poison");

        let mut passed = Vec::new();
        passthrough_reader
            .read_to_end(&mut passed)
            .expect("read passthrough");
        assert_eq!(passed, b"certain-output");
        assert_eq!(*tee.lock().expect("repaired tee"), b"certain-output");
        assert!(truncated.load(Ordering::SeqCst));
        assert!(pump_done.load(Ordering::SeqCst));
    }

    /// Serializes every test below that parks/takes/drains the process-global
    /// `PARKED_JOBS` registry. Without this, cargo test's default thread
    /// parallelism would let one test's `shutdown_stopped_jobs` (which drains
    /// the WHOLE registry) reap a still-in-flight job that a different test
    /// running concurrently just parked — a real cross-test race that lives in
    /// the test suite's shared global, not in the production code under test.
    static JOB_CONTROL_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// A PtyTee spec running `/bin/sh -c script`, no stdin — so `forward_tty`
    /// is always `false` and these tests need no controlling terminal (CI/
    /// agent sandboxes have none).
    fn sh_spec(script: &str) -> ExecSpec {
        ExecSpec {
            argv: vec![
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from(script),
            ],
            cwd: std::env::current_dir().expect("cwd"),
            env: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            stdin: StdinSpec::Null,
            mode: ExecMode::PtyTee,
            sandbox: None,
            spill: None,
        }
    }

    /// Poll `pred` until it is true or `timeout` elapses (panicking with `msg`
    /// on timeout). Used throughout instead of a fixed sleep so these tests
    /// bound their worst-case runtime without flaking under CI load.
    fn wait_until(timeout: Duration, msg: &str, mut pred: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            if pred() {
                return;
            }
            assert!(Instant::now() < deadline, "{msg}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// `true` once `pid` is no longer a live process at all — `kill(pid, 0)`
    /// fails with `ESRCH`. A merely *stopped* process still passes
    /// `kill(pid, 0)` (it's alive, just suspended), so this only goes true
    /// once the process has actually been reaped — exactly the distinction
    /// the no-zombie/no-orphan assertions below need.
    #[allow(clippy::cast_possible_wrap)] // pids fit in i32 in practice
    fn process_is_gone(pid: u32) -> bool {
        let pid = pid as libc::pid_t;
        // SAFETY: signal 0 is the POSIX existence probe; it delivers nothing.
        unsafe {
            libc::kill(pid, 0) == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        }
    }

    /// Raw-mode ownership is scoped, not process-lifetime state. In
    /// particular an evaluator/host panic while a PTY is foreground must not
    /// leave the user's terminal without echo or canonical input.
    #[test]
    fn raw_mode_guard_restores_termios_during_unwind() {
        struct Fds(libc::c_int, libc::c_int);
        impl Drop for Fds {
            fn drop(&mut self) {
                // SAFETY: both descriptors were returned by `openpty` and are
                // closed exactly once by this owner.
                unsafe {
                    libc::close(self.0);
                    libc::close(self.1);
                }
            }
        }

        let mut master = -1;
        let mut slave = -1;
        // SAFETY: valid output pointers; null optional name/termios/winsize
        // requests the platform defaults for a fresh pseudo-terminal.
        let opened = unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "openpty: {}", io::Error::last_os_error());
        let _fds = Fds(master, slave);

        // SAFETY: `slave` is the live terminal descriptor owned above.
        let mut before = unsafe { mem::zeroed::<libc::termios>() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &raw mut before) }, 0);
        let unwind = std::panic::catch_unwind(|| {
            let _raw = RawModeGuard::new(slave).expect("pty slave supports raw mode");
            // SAFETY: same live terminal descriptor.
            let mut during = unsafe { mem::zeroed::<libc::termios>() };
            assert_eq!(unsafe { libc::tcgetattr(slave, &raw mut during) }, 0);
            assert_eq!(during.c_lflag & (libc::ECHO | libc::ICANON), 0);
            panic!("exercise unwind restoration");
        });
        assert!(unwind.is_err());

        // SAFETY: same live terminal descriptor, after the guard was dropped.
        let mut after = unsafe { mem::zeroed::<libc::termios>() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &raw mut after) }, 0);
        assert_eq!(after.c_iflag, before.c_iflag);
        assert_eq!(after.c_oflag, before.c_oflag);
        assert_eq!(after.c_cflag, before.c_cflag);
        assert_eq!(after.c_lflag, before.c_lflag);
        assert_eq!(after.c_cc, before.c_cc);
    }

    #[test]
    fn raw_mode_setup_preserves_the_os_error() {
        let error = match RawModeGuard::new(-1) {
            Ok(_) => panic!("invalid terminal fd must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }

    /// The core job-control contract this module implements: a child that
    /// stops itself (`SIGSTOP`) is observed via `WUNTRACED` as a *stop*,
    /// `run_pty` maps that to a stopped `ExecResult` rather than an exit, and
    /// `SIGCONT` (driven here via `resume_foreground`) lets it run to
    /// completion normally. This is the exact mechanism a real Ctrl-Z rides on
    /// (the pty line discipline delivering `SIGTSTP` in place of our
    /// programmatic `SIGSTOP`); the one piece this cannot exercise without a
    /// real controlling terminal is that line-discipline translation itself —
    /// see the module docs and the corresponding comment in shoal-eval.
    #[test]
    fn wifstopped_maps_to_stopped_execresult_and_sigcont_resumes_to_completion() {
        let _serial = JOB_CONTROL_TEST_LOCK.lock().unwrap();
        let cancel = CancelToken::new();
        let res = run_pty(sh_spec("kill -STOP $$; echo resumed-ok"), &cancel).expect("run_pty");

        assert!(
            res.stopped,
            "a self-SIGSTOP under WUNTRACED must report stopped, not exited"
        );
        assert_eq!(res.status, None, "a stopped child has no exit status yet");
        assert_eq!(res.signal, None, "a stopped child has not died to a signal");
        assert_eq!(res.pgid, res.pid, "a pty child is its own group leader");

        let job = take_stopped_job(res.pid).expect("stopped job must be parked");
        assert_eq!(job.pid(), res.pid);

        let final_res = job
            .resume_foreground(&CancelToken::new())
            .expect("resume_foreground");
        assert!(
            !final_res.stopped,
            "a SIGCONT'd child that then exits is not stopped"
        );
        assert_eq!(final_res.status, Some(0));
        assert!(
            final_res.stdout.windows(10).any(|w| w == b"resumed-ok"),
            "expected the post-resume output in the tee, got {:?}",
            String::from_utf8_lossy(&final_res.stdout)
        );
    }

    /// `PARKED_JOBS` gives out each stopped job exactly once: a second
    /// `take_stopped_job` for the same pid must find nothing. The job it DID
    /// hand back, once dropped without resuming (an abandoned Ctrl-Z'd job),
    /// must leave no zombie/orphan behind — the `Drop` -> `kill_and_reap` path.
    #[test]
    fn take_stopped_job_is_exactly_once_and_drop_cleans_up() {
        let _serial = JOB_CONTROL_TEST_LOCK.lock().unwrap();
        let cancel = CancelToken::new();
        let res = run_pty(sh_spec("kill -STOP $$"), &cancel).expect("run_pty");
        assert!(res.stopped);

        let job = take_stopped_job(res.pid).expect("first take must find the parked job");
        assert!(
            take_stopped_job(res.pid).is_none(),
            "a parked job must not be handed out twice"
        );

        drop(job); // abandoned without resuming: Drop must reap it via kill_and_reap
        wait_until(
            Duration::from_secs(5),
            "an abandoned stopped job must be reaped on drop, not left as a zombie/orphan",
            || process_is_gone(res.pid),
        );
    }

    /// `shutdown_stopped_jobs` drains every parked job (not just one), killing
    /// and reaping each — the host calls this on shell shutdown so a stack of
    /// Ctrl-Z'd jobs never survives the process exiting.
    #[test]
    fn shutdown_stopped_jobs_drains_every_parked_job_without_a_leak() {
        let _serial = JOB_CONTROL_TEST_LOCK.lock().unwrap();
        let cancel = CancelToken::new();
        let a = run_pty(sh_spec("kill -STOP $$"), &cancel).expect("run_pty a");
        let b = run_pty(sh_spec("kill -STOP $$"), &cancel).expect("run_pty b");
        assert!(a.stopped && b.stopped);

        shutdown_stopped_jobs();

        assert!(
            take_stopped_job(a.pid).is_none(),
            "job a must have been drained by shutdown"
        );
        assert!(
            take_stopped_job(b.pid).is_none(),
            "job b must have been drained by shutdown"
        );
        wait_until(
            Duration::from_secs(5),
            "job a must be reaped by shutdown, not left running/orphaned",
            || process_is_gone(a.pid),
        );
        wait_until(
            Duration::from_secs(5),
            "job b must be reaped by shutdown, not left running/orphaned",
            || process_is_gone(b.pid),
        );
    }

    /// `kill_and_reap` is idempotent: once a job is marked `reaped`, a second
    /// call must be a safe no-op rather than re-signalling a pid the kernel
    /// may since have recycled for an unrelated process — the guard the
    /// `reaped` field exists for.
    #[test]
    fn kill_and_reap_is_idempotent_and_guards_an_already_reaped_job() {
        let _serial = JOB_CONTROL_TEST_LOCK.lock().unwrap();
        let cancel = CancelToken::new();
        let res = run_pty(sh_spec("kill -STOP $$"), &cancel).expect("run_pty");
        let mut job = take_stopped_job(res.pid).expect("parked job");

        assert!(
            !job.reaped,
            "a freshly parked job must not start out reaped"
        );
        job.kill_and_reap();
        assert!(job.reaped, "kill_and_reap must mark the job reaped");
        wait_until(
            Duration::from_secs(5),
            "kill_and_reap must actually kill and reap the child",
            || process_is_gone(res.pid),
        );

        // Second call on an already-reaped job: must not panic/error and must
        // remain a no-op (nothing left alive to mis-signal).
        job.kill_and_reap();
        assert!(job.reaped);
    }

    /// A resume can race with external termination (or an administrator
    /// killing the process group). The failed SIGCONT must be returned and,
    /// critically, every helper attached before that signal must still stop.
    #[test]
    fn failed_resume_signal_retires_serve_helpers_and_reaps() {
        let _serial = JOB_CONTROL_TEST_LOCK.lock().unwrap();
        let cancel = CancelToken::new();
        let res = run_pty(sh_spec("kill -STOP $$"), &cancel).expect("run_pty");
        let job = take_stopped_job(res.pid).expect("parked job");
        let pgid = job.pgid;

        // SAFETY: this is the stopped process group owned by `job`.
        assert_eq!(unsafe { libc::kill(-pgid, libc::SIGKILL) }, 0);
        waitpid_blocking(job.pid).expect("reap externally terminated child");
        assert!(process_is_gone(res.pid));

        let start = Instant::now();
        let error = job
            .resume_foreground(&cancel)
            .expect_err("SIGCONT of a vanished group must fail");
        assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "failed resume must not strand helper threads"
        );
        assert!(process_is_gone(res.pid), "the dead child must be reaped");
    }

    /// `resume_background`: `SIGCONT` lets the job finish off the calling
    /// thread (no fg terminal reattachment), and it is still reaped with no
    /// zombie left behind once it exits.
    #[test]
    fn resume_background_notifies_completion_and_is_reaped() {
        let _serial = JOB_CONTROL_TEST_LOCK.lock().unwrap();
        let cancel = CancelToken::new();
        let res = run_pty(sh_spec("kill -STOP $$"), &cancel).expect("run_pty");
        let job = take_stopped_job(res.pid).expect("parked job");
        let (tx, rx) = std::sync::mpsc::channel();

        job.resume_background_notify(move |result| {
            let _ = tx.send(result);
        });

        let completed = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("background completion notification")
            .expect("background resume result");
        assert!(!completed.stopped);
        assert_eq!(completed.status, Some(0));

        wait_until(
            Duration::from_secs(5),
            "a backgrounded job must run to completion and be reaped",
            || process_is_gone(res.pid),
        );
    }

    #[test]
    fn running_background_job_transfers_back_to_foreground_ownership() {
        let _serial = JOB_CONTROL_TEST_LOCK.lock().unwrap();
        let cancel = CancelToken::new();
        let res = run_pty(sh_spec("kill -STOP $$; sleep 30"), &cancel).expect("run_pty");
        let job = take_stopped_job(res.pid).expect("parked job");
        let (tx, rx) = std::sync::mpsc::channel();
        job.resume_background_notify(move |result| {
            let _ = tx.send(result);
        });

        let job = take_background_job(res.pid)
            .expect("ownership request")
            .expect("live background job");
        assert_eq!(job.pid(), res.pid);
        assert!(
            matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
            "ownership transfer is not a terminal completion notification"
        );

        let foreground_cancel = CancelToken::new();
        foreground_cancel.cancel();
        let completed = job
            .foreground_running(&foreground_cancel)
            .expect("foreground attach and cancellation");
        assert!(!completed.stopped);
        assert_eq!(completed.signal.as_deref(), Some("SIGINT"));
        assert!(process_is_gone(res.pid));
        assert!(
            take_background_job(res.pid).unwrap().is_none(),
            "transferred job must leave the background registry"
        );
    }

    #[test]
    fn background_output_can_be_routed_to_a_host_sink() {
        let _serial = JOB_CONTROL_TEST_LOCK.lock().unwrap();
        let cancel = CancelToken::new();
        let res =
            run_pty(sh_spec("kill -STOP $$; printf background-safe"), &cancel).expect("run_pty");
        let job = take_stopped_job(res.pid).expect("parked job");
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        let (tx, rx) = std::sync::mpsc::channel();

        job.resume_background_notify_with_output(
            move |bytes| sink.lock().unwrap().extend_from_slice(bytes),
            move |result| {
                let _ = tx.send(result);
            },
        );

        let completed = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("background completion")
            .expect("background result");
        assert_eq!(completed.status, Some(0));
        assert!(
            output
                .lock()
                .unwrap()
                .windows(b"background-safe".len())
                .any(|window| window == b"background-safe")
        );
        assert!(process_is_gone(res.pid));
    }

    #[test]
    fn shutdown_reaps_a_running_background_job() {
        let _serial = JOB_CONTROL_TEST_LOCK.lock().unwrap();
        let cancel = CancelToken::new();
        let res = run_pty(sh_spec("kill -STOP $$; sleep 30"), &cancel).expect("run_pty");
        let job = take_stopped_job(res.pid).expect("parked job");
        job.resume_background();

        shutdown_stopped_jobs();
        wait_until(
            Duration::from_secs(5),
            "shell shutdown must reap running background PTYs",
            || process_is_gone(res.pid),
        );
    }

    #[test]
    fn production_pty_ownership_stays_decomposed() {
        let root = include_str!("pty.rs");
        let production_root = root
            .split_once("#[cfg(test)]")
            .map_or(root, |(production, _)| production);
        let registry = include_str!("pty/registry.rs");
        let service = include_str!("pty/service.rs");
        let terminal = include_str!("pty/terminal.rs");

        assert!(
            production_root.lines().count() <= 430,
            "PTY production root grew past its orchestration boundary"
        );
        assert!(
            registry.lines().count() <= 120,
            "PTY registry grew too broad"
        );
        assert!(service.lines().count() <= 250, "PTY service grew too broad");
        assert!(
            terminal.lines().count() <= 300,
            "PTY terminal I/O grew too broad"
        );

        assert!(
            !production_root.contains("libc::poll")
                && !production_root.contains("libc::tcgetattr")
                && !production_root.contains("static PARKED_JOBS")
                && !production_root.contains("static BACKGROUND_JOBS"),
            "OS pumps and process-global registries belong in owned submodules"
        );

        let run_pty = production_root
            .split_once("pub(crate) fn run_pty")
            .expect("run_pty remains in the orchestration root")
            .1;
        assert!(
            run_pty.lines().count() <= 125,
            "run_pty should remain spawn-and-dispatch orchestration"
        );
        let serve_span = service
            .split_once("pub(super) fn serve")
            .expect("service entrypoint")
            .1
            .split_once("\nfn attach_input")
            .expect("input helper boundary")
            .0;
        assert!(
            serve_span.lines().count() <= 65,
            "one service stint should delegate helper ownership"
        );
    }
}
