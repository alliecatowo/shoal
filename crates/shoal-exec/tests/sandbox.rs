use shoal_exec::{
    CancelToken, ExecMode, ExecSpec, StdinSpec, run, run_sandboxed, spawn_capture,
    spawn_capture_sandboxed,
};
use shoal_leash::{EnforcementTier, FsSandbox, NetPolicy, SandboxPolicy, SpawnPreflight};
use std::io::Read;
use std::time::{Duration, Instant};
fn spec(script: &str, mode: ExecMode) -> ExecSpec {
    ExecSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: std::env::current_dir().unwrap(),
        env: std::env::vars_os().collect(),
        stdin: StdinSpec::Null,
        mode,
        sandbox: None,
        spill: None,
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
// ---------------------------------------------------- ExecSpec.sandbox path
//
// The tests above exercise the older run_sandboxed/spawn_capture_sandboxed
// functions. These prove the SAME OS enforcement fires through the pinned
// `run`/`spawn_capture` entry points once `ExecSpec.sandbox` is populated —
// the wiring the audit found missing.

fn sandboxed(script: &str, mode: ExecMode, policy: SandboxPolicy) -> ExecSpec {
    let mut s = spec(script, mode);
    s.sandbox = Some(policy);
    s
}

#[test]
fn execspec_sandbox_allows_one_file_and_denies_sibling_via_run() {
    if shoal_leash::landlock_abi().is_none() {
        eprintln!("Landlock unavailable; skipping enforcement assertion");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let a = d.path().join("allowed");
    let b = d.path().join("denied");
    std::fs::write(&a, "ok").unwrap();
    std::fs::write(&b, "no").unwrap();
    let script = format!("cat '{}' && ! cat '{}'", a.display(), b.display());
    for mode in [ExecMode::Capture, ExecMode::PtyTee] {
        let policy = SandboxPolicy {
            fs: grants(a.clone()),
            net: NetPolicy::Unrestricted,
            spawn_hash: None,
            hermetic: false,
        };
        let r = run(sandboxed(&script, mode, policy), &CancelToken::new()).unwrap();
        assert_eq!(r.status, Some(0), "{}", String::from_utf8_lossy(&r.stderr));
        assert!(r.stdout.windows(2).any(|x| x == b"ok"));

        // Honesty: the caller can see what actually got applied, not just
        // that the command happened to succeed.
        let st = r.enforcement.expect("sandbox was requested");
        assert!(st.enforced);
        assert_eq!(st.active_tier, Some(EnforcementTier::A));
        assert!(st.filesystem_enforced);
    }
}

#[test]
fn execspec_sandbox_via_spawn_capture_reports_enforcement_on_wait() {
    if shoal_leash::landlock_abi().is_none() {
        eprintln!("Landlock unavailable; skipping enforcement assertion");
        return;
    }
    let policy = SandboxPolicy {
        fs: grants("/bin/sh".into()),
        net: NetPolicy::Unrestricted,
        spawn_hash: None,
        hermetic: false,
    };
    let token = CancelToken::new();
    let mut child = spawn_capture(sandboxed("printf ok", ExecMode::Capture, policy), &token)
        .expect("spawn_capture with sandbox");
    let mut out = Vec::new();
    let _ = child.stdout.read_to_end(&mut out);
    let r = child.wait(&token).unwrap();
    assert_eq!(out, b"ok");
    let st = r.enforcement.expect("sandbox was requested");
    assert!(st.enforced);
    assert_eq!(st.active_tier, Some(EnforcementTier::A));
}

#[test]
fn execspec_sandbox_degrades_honestly_when_no_backend_is_available() {
    // The degrade path is only reachable where NO OS backend exists. macOS
    // always has the Seatbelt backend (so a requested sandbox really confines
    // the child), and Linux with Landlock likewise — skip in either case.
    if shoal_leash::landlock_abi().is_some() || cfg!(target_os = "macos") {
        eprintln!("an OS sandbox backend is available; degrade path not reachable, skip");
        return;
    }
    // No Landlock (older kernel / container that blocks the syscall): the
    // best-effort (non-hermetic) request must still run the child rather
    // than breaking the shell, but must NOT claim enforcement happened.
    let policy = SandboxPolicy {
        fs: grants("/bin/sh".into()),
        net: NetPolicy::Unrestricted,
        spawn_hash: None,
        hermetic: false,
    };
    let r = run(
        sandboxed("printf ok", ExecMode::Capture, policy),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(r.stdout, b"ok", "degraded run must still execute the child");
    let st = r.enforcement.expect("sandbox was requested");
    assert!(!st.enforced, "must not claim enforcement it did not apply");
    assert_eq!(st.active_tier, None);
    assert!(st.detail.contains("WITHOUT OS confinement"));
}

#[test]
fn execspec_sandbox_hermetic_fails_closed_when_net_deny_cannot_be_enforced() {
    // net.deny has no enforcement backend anywhere in this crate today, on
    // any platform — so a `hermetic: true` request for it must always
    // refuse to spawn rather than silently run with network still open.
    let policy = SandboxPolicy {
        fs: FsSandbox::default(),
        net: NetPolicy::Deny,
        spawn_hash: None,
        hermetic: true,
    };
    let e = run(
        sandboxed("true", ExecMode::Capture, policy),
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn execspec_sandbox_hermetic_refuses_an_unresolved_filesystem_scope() {
    let policy = SandboxPolicy {
        fs: FsSandbox::default(),
        net: NetPolicy::Unrestricted,
        spawn_hash: None,
        hermetic: true,
    };
    let e = run(
        sandboxed("true", ExecMode::Capture, policy),
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(e.to_string().contains("no usable roots"));
}

#[test]
fn execspec_sandbox_hermetic_refuses_preexec_identity_pinning() {
    let policy = SandboxPolicy {
        fs: grants("/bin/sh".into()),
        net: NetPolicy::Unrestricted,
        spawn_hash: Some("00".repeat(32)),
        hermetic: true,
    };
    let e = run(
        sandboxed("true", ExecMode::Capture, policy),
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
    assert!(e.to_string().contains("TOCTOU"));
}

#[test]
fn execspec_sandbox_spawn_hash_pin_matches_and_mismatches() {
    if shoal_leash::landlock_abi().is_none() {
        eprintln!("Landlock unavailable; skipping enforcement assertion");
        return;
    }
    let real = shoal_leash::preflight_spawn(std::path::Path::new("/bin/sh"), &[]).unwrap();

    // Correct pin: runs, and the result says the pin was checked.
    let ok_policy = SandboxPolicy {
        fs: grants("/bin/sh".into()),
        net: NetPolicy::Unrestricted,
        spawn_hash: Some(real.hash.clone()),
        hermetic: false,
    };
    let r = run(
        sandboxed("printf ok", ExecMode::Capture, ok_policy),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(r.stdout, b"ok");
    assert!(r.enforcement.unwrap().spawn_exec_enforced);

    // Wrong pin: refused before spawn (proc.spawn hash pin, site/content/internals/language-conformance-contract.md).
    let bad_policy = SandboxPolicy {
        fs: grants("/bin/sh".into()),
        net: NetPolicy::Unrestricted,
        spawn_hash: Some("00".repeat(32)),
        hermetic: false,
    };
    let e = run(
        sandboxed(
            "touch /tmp/must-not-exist-shoal-pin",
            ExecMode::Capture,
            bad_policy,
        ),
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(!std::path::Path::new("/tmp/must-not-exist-shoal-pin").exists());
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
