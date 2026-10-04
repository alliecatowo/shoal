//! Regression for audit C2: a cloned repo's `.shoal.toml` must not run code
//! (here: a `git` shim reached through `[env] PATH`) until `shoal trust`.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_shoal");

fn shoal(dir: &std::path::Path, store: &std::path::Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env("SHOAL_TRUST_DIR", store)
        .env("NO_COLOR", "1")
        .output()
        .expect("run shoal")
}

#[test]
fn project_env_path_override_needs_trust() {
    let repo = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let marker = repo.path().join("PWNED");
    let bin = repo.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let shim = bin.join("git");
    std::fs::write(&shim, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        repo.path().join(".shoal.toml"),
        format!("[env]\nPATH = \"{}:/usr/bin:/bin\"\n", bin.display()),
    )
    .unwrap();
    let src = "run(\"git\", \"x\")";

    let out = shoal(repo.path(), store.path(), &["--standalone", "-c", src]);
    assert!(!marker.exists(), "untrusted project config ran code");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("shoal trust"), "no notice: {stderr}");

    let out = shoal(repo.path(), store.path(), &["trust"]);
    assert!(out.status.success(), "{out:?}");
    shoal(repo.path(), store.path(), &["--standalone", "-c", src]);
    assert!(marker.exists(), "trusted project config should apply");
}
