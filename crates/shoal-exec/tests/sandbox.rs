use shoal_exec::{
    CancelToken, ExecMode, ExecSpec, StdinSpec, run_sandboxed, spawn_capture_sandboxed,
};
use shoal_leash::{FsSandbox, SpawnPreflight};
use std::io::Read;
use std::time::{Duration, Instant};
fn spec(script: &str, mode: ExecMode) -> ExecSpec {
    ExecSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: std::env::current_dir().unwrap(),
        env: std::env::vars_os().collect(),
        stdin: StdinSpec::Null,
        mode,
    }
}
fn grants(path: std::path::PathBuf) -> FsSandbox {
    let mut read = vec![path];
    for p in ["/bin", "/usr", "/lib", "/lib64", "/etc/ld.so.cache"] {
        let p = std::path::PathBuf::from(p);
        if p.exists() {
            read.push(p)
        }
    }
    FsSandbox {
        read,
        write: vec![],
        delete: vec![],
    }
}
#[test]
fn sandboxed_capture_and_pty_allow_one_file_deny_sibling() {
    if shoal_leash::landlock_abi().is_none() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let a = d.path().join("allowed");
    let b = d.path().join("denied");
    std::fs::write(&a, "ok").unwrap();
    std::fs::write(&b, "no").unwrap();
    let script = format!("cat '{}' && ! cat '{}'", a.display(), b.display());
    for mode in [ExecMode::Capture, ExecMode::PtyTee] {
        let r = run_sandboxed(
            spec(&script, mode),
            &CancelToken::new(),
            grants(a.clone()),
            None,
        )
        .unwrap();
        assert_eq!(r.status, Some(0), "{}", String::from_utf8_lossy(&r.stderr));
        assert!(r.stdout.windows(2).any(|x| x == b"ok"));
    }
}
#[test]
fn wrong_verified_hash_rejected_before_spawn() {
    if shoal_leash::landlock_abi().is_none() {
        return;
    }
    let fake = SpawnPreflight {
        hash: "00".repeat(32),
        allowed: true,
        assurance: "test",
    };
    let e = run_sandboxed(
        spec("touch /tmp/must-not-exist-shoal", ExecMode::Capture),
        &CancelToken::new(),
        FsSandbox::default(),
        Some(&fake),
    )
    .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied)
}
#[test]
fn sandbox_stream_cancellation_reaps() {
    if shoal_leash::landlock_abi().is_none() {
        return;
    }
    let token = CancelToken::new();
    let mut child = spawn_capture_sandboxed(
        spec("sleep 30", ExecMode::Capture),
        &token,
        grants("/bin/sh".into()),
        None,
    )
    .unwrap();
    let pid = child.pid as i32;
    let t = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        t.cancel();
    });
    let start = Instant::now();
    let mut sink = Vec::new();
    let _ = child.stdout.read_to_end(&mut sink);
    let r = child.wait(&token).unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(r.signal.as_deref(), Some("SIGINT"));
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}
