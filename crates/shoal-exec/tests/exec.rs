//! End-to-end tests for shoal-exec against real processes (`/bin/sh`,
//! `/bin/cat`, coreutils). PTY assertions are limited to what holds even when
//! the test harness itself has no tty: the child sees a pty, bytes are teed.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use shoal_exec::{
    CancelToken, ExecMode, ExecSpec, StdinSpec, run, spawn_capture, stream_stdin, which,
};

/// Minimal self-cleaning temp dir (avoids a tempfile dependency).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "shoal-exec-test-{}-{}-{tag}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace(':', "_"),
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spec(argv: &[&str], mode: ExecMode) -> ExecSpec {
    ExecSpec {
        argv: argv.iter().map(OsString::from).collect(),
        cwd: std::env::current_dir().expect("cwd"),
        env: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
        stdin: StdinSpec::Null,
        mode,
        sandbox: None,
        spill: None,
    }
}

fn sh(script: &str, mode: ExecMode) -> ExecSpec {
    spec(&["/bin/sh", "-c", script], mode)
}

fn cancel_after(token: &CancelToken, delay: Duration) {
    let t = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        t.cancel();
    });
}

// ---------------------------------------------------------------- capture

#[test]
fn capture_stdout_stderr_and_exit_zero() {
    let res = run(
        sh("printf out; printf err >&2", ExecMode::Capture),
        &CancelToken::new(),
    )
    .expect("run");
    assert_eq!(res.status, Some(0));
    assert_eq!(res.signal, None);
    assert_eq!(res.stdout, b"out");
    assert_eq!(res.stderr, b"err");
    assert!(res.pid > 0);
}

#[test]
fn capture_nonzero_exit_code() {
    let res = run(sh("exit 7", ExecMode::Capture), &CancelToken::new()).expect("run");
    assert_eq!(res.status, Some(7));
    assert_eq!(res.signal, None);
}

/// Deadlock regression: >1 MiB written to BOTH streams, alternating, so a
/// runner that drains one pipe at a time wedges on a full pipe buffer.
#[test]
fn capture_large_output_on_both_streams_does_not_deadlock() {
    let script = "i=0; while [ $i -lt 20 ]; do \
                  head -c 65536 /dev/zero; head -c 65536 /dev/zero >&2; \
                  i=$((i+1)); done";
    let res = run(sh(script, ExecMode::Capture), &CancelToken::new()).expect("run");
    assert_eq!(res.status, Some(0));
    assert_eq!(res.stdout.len(), 20 * 65536);
    assert_eq!(res.stderr.len(), 20 * 65536);
    assert!(res.stdout.len() > 1 << 20, "must exceed 1MiB per stream");
}

#[test]
fn capture_stdin_bytes_are_fed_and_closed() {
    let mut s = spec(&["/bin/cat"], ExecMode::Capture);
    s.stdin = StdinSpec::Bytes(b"hello stdin".to_vec());
    let res = run(s, &CancelToken::new()).expect("run");
    assert_eq!(res.status, Some(0));
    assert_eq!(res.stdout, b"hello stdin");
}

#[test]
fn capture_stream_stdin_consumes_bounded_chunks_until_producer_close() {
    let (sink, stdin) = stream_stdin(1);
    let mut s = spec(&["/bin/cat"], ExecMode::Capture);
    s.stdin = stdin;
    let producer = std::thread::spawn(move || {
        sink.try_send(b"first\n".to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        sink.try_send(b"second\n".to_vec()).unwrap();
    });
    let res = run(s, &CancelToken::new()).expect("run");
    producer.join().unwrap();
    assert_eq!(res.status, Some(0));
    assert_eq!(res.stdout, b"first\nsecond\n");
}

#[test]
fn stream_stdin_capacity_applies_before_execution_claims_the_receiver() {
    let (sink, _stdin) = stream_stdin(1);
    sink.try_send(vec![1]).unwrap();
    assert!(matches!(
        sink.try_send(vec![2]),
        Err(std::sync::mpsc::TrySendError::Full(_))
    ));
}

#[test]
fn capture_stdin_file() {
    let dir = TempDir::new("stdin-file");
    let file = dir.path().join("input.txt");
    std::fs::write(&file, b"from a file").expect("write fixture");
    let mut s = spec(&["/bin/cat"], ExecMode::Capture);
    s.stdin = StdinSpec::File(file);
    let res = run(s, &CancelToken::new()).expect("run");
    assert_eq!(res.status, Some(0));
    assert_eq!(res.stdout, b"from a file");
}

#[test]
fn capture_stdin_null_gives_immediate_eof() {
    let res = run(spec(&["/bin/cat"], ExecMode::Capture), &CancelToken::new()).expect("run");
    assert_eq!(res.status, Some(0));
    assert!(res.stdout.is_empty());
}

#[test]
fn capture_respects_cwd() {
    let dir = TempDir::new("cwd");
    let canon = dir.path().canonicalize().expect("canonicalize");
    let mut s = sh("pwd", ExecMode::Capture);
    s.cwd = canon.clone();
    let res = run(s, &CancelToken::new()).expect("run");
    assert_eq!(
        String::from_utf8_lossy(&res.stdout).trim(),
        canon.to_string_lossy()
    );
}

#[test]
fn capture_env_is_complete_and_exact() {
    let mut s = sh(r#"printf "%s" "$FOO""#, ExecMode::Capture);
    s.env.push((OsString::from("FOO"), OsString::from("bar")));
    let res = run(s, &CancelToken::new()).expect("run");
    assert_eq!(res.stdout, b"bar");
}

// ------------------------------------------------------------ wait status

#[test]
fn signal_death_reports_sigsegv_by_name() {
    let res = run(sh("kill -SEGV $$", ExecMode::Capture), &CancelToken::new()).expect("run");
    assert_eq!(res.status, None, "signal death must not fake an exit code");
    assert_eq!(res.signal.as_deref(), Some("SIGSEGV"));
}

#[test]
fn signal_death_reports_sigkill_by_name() {
    let res = run(sh("kill -KILL $$", ExecMode::Capture), &CancelToken::new()).expect("run");
    assert_eq!(res.status, None);
    assert_eq!(res.signal.as_deref(), Some("SIGKILL"));
}

// ------------------------------------------------------------ cancellation

#[test]
fn cancel_interrupts_a_sleeping_child_quickly() {
    let token = CancelToken::new();
    cancel_after(&token, Duration::from_millis(100));
    let start = Instant::now();
    let res = run(spec(&["/bin/sleep", "30"], ExecMode::Capture), &token).expect("run");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "cancellation took {elapsed:?}, expected well under the sleep"
    );
    assert_eq!(res.signal.as_deref(), Some("SIGINT"));
    assert_eq!(res.status, None);
}

#[test]
fn cancel_escalates_to_sigterm_when_sigint_is_ignored() {
    let token = CancelToken::new();
    cancel_after(&token, Duration::from_millis(100));
    let start = Instant::now();
    let res = run(sh("trap '' INT; sleep 30", ExecMode::Capture), &token).expect("run");
    let elapsed = start.elapsed();
    assert_eq!(res.signal.as_deref(), Some("SIGTERM"));
    assert!(
        elapsed >= Duration::from_millis(2500) && elapsed < Duration::from_secs(10),
        "SIGTERM should land after the ~3s grace, took {elapsed:?}"
    );
}

#[test]
fn cancel_escalates_to_sigkill_when_int_and_term_are_ignored() {
    let token = CancelToken::new();
    cancel_after(&token, Duration::from_millis(100));
    let start = Instant::now();
    let res = run(sh("trap '' INT TERM; sleep 30", ExecMode::Capture), &token).expect("run");
    let elapsed = start.elapsed();
    assert_eq!(res.signal.as_deref(), Some("SIGKILL"));
    assert!(
        elapsed >= Duration::from_millis(5500) && elapsed < Duration::from_secs(15),
        "SIGKILL should land after both 3s graces, took {elapsed:?}"
    );
}

#[test]
fn pre_cancelled_token_kills_immediately() {
    let token = CancelToken::new();
    token.cancel();
    assert!(token.is_cancelled());
    let start = Instant::now();
    let res = run(spec(&["/bin/sleep", "30"], ExecMode::Capture), &token).expect("run");
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(res.signal.as_deref(), Some("SIGINT"));
}

// -------------------------------------------------------------- streaming

#[test]
fn streaming_child_can_be_drained_then_waited() {
    let token = CancelToken::new();
    let mut child = spawn_capture(sh("printf streamed", ExecMode::Capture), &token).expect("spawn");
    let mut buf = Vec::new();
    child.stdout.read_to_end(&mut buf).expect("drain stdout");
    assert_eq!(buf, b"streamed");
    let res = child.wait(&token).expect("wait");
    assert_eq!(res.status, Some(0));
    assert!(res.stdout.is_empty(), "caller drained; result stays empty");
}

#[test]
fn streaming_child_cancel_unblocks_a_draining_reader() {
    let token = CancelToken::new();
    let mut child = spawn_capture(
        sh("printf start; sleep 30; printf end", ExecMode::Capture),
        &token,
    )
    .expect("spawn");
    cancel_after(&token, Duration::from_millis(300));
    let start = Instant::now();
    let mut buf = Vec::new();
    // Blocks until the watcher (armed at spawn time) kills the child.
    let _ = child.stdout.read_to_end(&mut buf);
    let res = child.wait(&token).expect("wait");
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(buf, b"start");
    assert_eq!(res.signal.as_deref(), Some("SIGINT"));
}

#[test]
fn streaming_child_wait_honors_a_different_token() {
    let spawn_token = CancelToken::new();
    let child =
        spawn_capture(spec(&["/bin/sleep", "30"], ExecMode::Capture), &spawn_token).expect("spawn");
    let wait_token = CancelToken::new();
    cancel_after(&wait_token, Duration::from_millis(100));
    let start = Instant::now();
    let res = child.wait(&wait_token).expect("wait");
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(res.signal.as_deref(), Some("SIGINT"));
}

#[test]
fn spawn_capture_rejects_pty_mode() {
    let err = match spawn_capture(sh("true", ExecMode::PtyTee), &CancelToken::new()) {
        Ok(_) => panic!("PTY mode must not be accepted by the capture-only API"),
        Err(err) => err,
    };
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn dropping_a_streaming_child_reaps_it() {
    let child = spawn_capture(
        spec(&["/bin/sleep", "30"], ExecMode::Capture),
        &CancelToken::new(),
    )
    .expect("spawn");
    let pid = child.pid as i32;
    drop(child); // must SIGKILL and reap — no zombie left behind
    // SAFETY: probing with signal 0 never delivers a signal.
    let alive = unsafe { libc::kill(pid, 0) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    assert_eq!(alive, -1, "pid should be gone (not even a zombie)");
    assert_eq!(errno, Some(libc::ESRCH));
}

// ------------------------------------------------------------ spawn errors

#[test]
fn unresolvable_command_is_not_found() {
    let err = run(
        spec(&["definitely-not-a-command-xyz"], ExecMode::Capture),
        &CancelToken::new(),
    )
    .expect_err("must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

// Relies on Linux's per-argument MAX_ARG_STRLEN (128 KiB); macOS has no such
// per-arg cap, so a 200 KiB single argument spawns fine there. The general
// contract (spawn failure surfaces as io::Error) holds cross-platform, but is
// only cheaply triggerable on Linux.
#[cfg(target_os = "linux")]
#[test]
fn e2big_surfaces_as_io_error() {
    // A single argument beyond Linux's MAX_ARG_STRLEN (128 KiB) → E2BIG.
    let huge = "x".repeat(200_000);
    let err = run(
        spec(&["/bin/true", &huge], ExecMode::Capture),
        &CancelToken::new(),
    )
    .expect_err("must fail");
    assert_eq!(err.raw_os_error(), Some(libc::E2BIG));
}

#[test]
fn empty_argv_is_invalid_input() {
    let s = ExecSpec {
        argv: vec![],
        cwd: std::env::current_dir().expect("cwd"),
        env: vec![],
        stdin: StdinSpec::Null,
        mode: ExecMode::Capture,
        sandbox: None,
        spill: None,
    };
    let err = run(s, &CancelToken::new()).expect_err("must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

// ------------------------------------------------------------------ which

fn write_script(dir: &Path, name: &str, mode: u32) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, "#!/bin/sh\nprintf resolved-ok\n").expect("write script");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).expect("chmod");
    p
}

#[test]
fn which_finds_executables_and_rejects_the_rest() {
    let dir = TempDir::new("which");
    let hit = write_script(dir.path(), "toolx", 0o755);
    write_script(dir.path(), "not-exec", 0o644);
    let path_var = OsString::from(format!("/definitely/absent:{}", dir.path().display()));

    assert_eq!(which(OsStr::new("toolx"), Some(&path_var)), Some(hit));
    assert_eq!(which(OsStr::new("not-exec"), Some(&path_var)), None);
    assert_eq!(which(OsStr::new("missing-entirely"), Some(&path_var)), None);
    assert_eq!(which(OsStr::new(""), Some(&path_var)), None);
}

#[test]
fn which_checks_slash_names_directly() {
    assert_eq!(
        which(OsStr::new("/bin/sh"), None),
        Some(PathBuf::from("/bin/sh"))
    );
    assert_eq!(which(OsStr::new("/bin/no-such-binary-zzz"), None), None);
}

#[test]
fn which_falls_back_to_process_path() {
    // The test environment always has a shell on PATH.
    assert!(which(OsStr::new("sh"), None).is_some());
}

#[test]
fn run_resolves_argv0_via_the_spec_env_path() {
    let dir = TempDir::new("resolve");
    write_script(dir.path(), "mytool", 0o755);
    let mut s = spec(&["mytool"], ExecMode::Capture);
    s.env = vec![(
        OsString::from("PATH"),
        OsString::from(format!("{}:/usr/bin:/bin", dir.path().display())),
    )];
    let res = run(s, &CancelToken::new()).expect("run");
    assert_eq!(res.status, Some(0));
    assert_eq!(res.stdout, b"resolved-ok");
}

// -------------------------------------------------------------------- pty

#[test]
fn pty_child_sees_a_tty_on_all_std_fds() {
    let res = run(
        sh(
            "test -t 0 && test -t 1 && test -t 2 && printf ISATTY",
            ExecMode::PtyTee,
        ),
        &CancelToken::new(),
    )
    .expect("run");
    assert_eq!(res.status, Some(0), "child must see a tty even in CI");
    assert!(
        res.stdout.windows(6).any(|w| w == b"ISATTY"),
        "teed bytes must contain the marker; got {:?}",
        String::from_utf8_lossy(&res.stdout)
    );
}

#[test]
fn pty_child_is_its_own_process_group_leader() {
    // Job control (site/content/internals/language-conformance-contract.md) signals the whole group via `kill(-pgid, …)`, so
    // the child must be in its OWN group. portable-pty's `setsid` makes it a
    // session/group leader, so pgid == pid — both positive.
    let res = run(sh("true", ExecMode::PtyTee), &CancelToken::new()).expect("run");
    assert!(res.pid > 0, "child must report a pid");
    assert_eq!(
        res.pgid, res.pid,
        "a PTY child is a session leader, so its process-group id is its pid"
    );
    assert!(
        !res.stopped,
        "a child that ran to completion is not stopped"
    );
}

#[test]
fn pty_tee_captures_output_bytes() {
    let res = run(
        sh("printf hello-from-pty", ExecMode::PtyTee),
        &CancelToken::new(),
    )
    .expect("run");
    assert_eq!(res.status, Some(0));
    assert_eq!(res.stdout, b"hello-from-pty");
    assert!(res.stderr.is_empty(), "pty result never carries stderr");
}

#[test]
fn pty_merges_stderr_into_the_teed_stream() {
    let res = run(
        sh("printf err-on-pty >&2", ExecMode::PtyTee),
        &CancelToken::new(),
    )
    .expect("run");
    assert_eq!(res.status, Some(0));
    assert_eq!(res.stdout, b"err-on-pty");
    assert!(res.stderr.is_empty());
}

#[test]
fn pty_reports_exit_codes() {
    let res = run(sh("exit 3", ExecMode::PtyTee), &CancelToken::new()).expect("run");
    assert_eq!(res.status, Some(3));
    assert_eq!(res.signal, None);
}

#[test]
fn pty_reports_signal_deaths_by_name() {
    let res = run(sh("kill -SEGV $$", ExecMode::PtyTee), &CancelToken::new()).expect("run");
    assert_eq!(res.status, None);
    assert_eq!(res.signal.as_deref(), Some("SIGSEGV"));
}

#[test]
fn pty_cancel_interrupts_a_sleeping_child() {
    let token = CancelToken::new();
    cancel_after(&token, Duration::from_millis(200));
    let start = Instant::now();
    let res = run(spec(&["/bin/sleep", "30"], ExecMode::PtyTee), &token).expect("run");
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(res.signal.as_deref(), Some("SIGINT"));
}

#[test]
fn pty_stdin_bytes_drive_an_interactive_child() {
    let mut s = spec(&["/bin/cat"], ExecMode::PtyTee);
    // cat on a pty echoes what it reads; ^D alone on a line makes it exit.
    s.stdin = StdinSpec::Bytes(b"ping\n\x04".to_vec());
    let res = run(s, &CancelToken::new()).expect("run");
    assert_eq!(res.status, Some(0));
    assert!(
        res.stdout.windows(4).any(|w| w == b"ping"),
        "expected the echoed bytes in the tee, got {:?}",
        String::from_utf8_lossy(&res.stdout)
    );
}

#[test]
fn pty_stream_stdin_delivers_eof_when_the_producer_finishes() {
    let (sink, stdin) = stream_stdin(2);
    sink.try_send(b"streamed\n".to_vec()).unwrap();
    drop(sink);
    let mut s = spec(&["/bin/cat"], ExecMode::PtyTee);
    s.stdin = stdin;
    let res = run(s, &CancelToken::new()).expect("run");
    assert_eq!(res.status, Some(0));
    assert!(res.stdout.windows(8).any(|w| w == b"streamed"));
}

#[test]
fn pty_not_found_still_errors_cleanly() {
    let err = run(
        spec(&["definitely-not-a-command-xyz"], ExecMode::PtyTee),
        &CancelToken::new(),
    )
    .expect_err("must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[cfg(target_os = "linux")]
#[test]
fn pty_e2big_preserves_the_os_error() {
    let huge = "x".repeat(200_000);
    let err = run(
        spec(&["/bin/true", &huge], ExecMode::PtyTee),
        &CancelToken::new(),
    )
    .expect_err("must fail");
    assert_eq!(err.raw_os_error(), Some(libc::E2BIG));
}
