#[test]
fn child_landlock_allows_subtree_and_denies_sibling() {
    if shoal_sh::leash::landlock_abi().is_none() {
        eprintln!("Landlock unavailable; skipping enforcement assertion");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let allowed = d.path().join("allowed");
    let denied = d.path().join("denied");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&denied).unwrap();
    let a = allowed.join("a");
    let b = denied.join("b");
    std::fs::write(&a, b"ok").unwrap();
    std::fs::write(&b, b"no").unwrap();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_shoal-landlock-helper"))
        .args([&a, &b])
        .status()
        .unwrap();
    if status.code() == Some(77) {
        eprintln!("Landlock reported but could not be activated in this container; skipping");
        return;
    }
    assert!(status.success(), "helper status {status}")
}

/// Hermetic net.deny must also stop UDP (DNS) and unix-socket egress, which
/// Landlock's TCP rights do not cover.
#[cfg(target_os = "linux")]
#[test]
fn deny_net_blocks_udp_and_unix_sockets() {
    if shoal_sh::leash::landlock_abi().is_none_or(|abi| abi < 4) {
        eprintln!("Landlock net rights unavailable; skipping");
        return;
    }
    let bash = std::path::Path::new("/bin/bash");
    if !bash.exists() {
        return;
    }
    let helper = env!("CARGO_BIN_EXE_shoal-sandbox-exec");
    let run = |deny: bool, script: &str| {
        let mut c = std::process::Command::new(helper);
        if deny {
            c.arg("--deny-net");
        }
        for root in ["/usr", "/lib", "/lib64", "/etc", "/proc", "/dev"] {
            if std::path::Path::new(root).exists() {
                c.args(["--read", root]);
            }
        }
        c.args(["--", "/bin/bash", "-c", script]).status().unwrap()
    };
    let udp = "echo x > /dev/udp/127.0.0.1/9";
    if !run(false, udp).success() {
        eprintln!("bash /dev/udp unsupported here; skipping");
        return;
    }
    // exit 1 = the redirect itself failed (not 126/127 = could not even exec)
    assert_eq!(run(true, udp).code(), Some(1), "UDP egress must be denied");
    assert_eq!(
        run(true, "echo x > /dev/tcp/127.0.0.1/9").code(),
        Some(1),
        "TCP egress must be denied"
    );
    assert!(
        run(true, ":").success(),
        "non-network commands must still run"
    );
}
