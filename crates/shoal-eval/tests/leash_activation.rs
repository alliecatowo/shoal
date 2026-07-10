//! Leash activation through the evaluator spawn path (TDD §8).
//!
//! These tests prove the wiring the audit found missing: a leash policy set on
//! the `Evaluator` (via `set_leash_policy`) is turned into a real OS sandbox
//! (`ExecSpec.sandbox`) at every external spawn.
//!
//! Two guarantees are covered:
//!   * ZERO REGRESSION — the default-permissive policy resolves to no OS
//!     confinement, so a normal command runs exactly as before.
//!   * REAL ENFORCEMENT — a genuinely-scoped policy blocks a spawned child from
//!     reading a filesystem path the policy did not grant.
//!
//! The Linux enforcement assertion is guarded on Landlock availability so it
//! skips cleanly on older kernels / containers that block the syscall. The
//! macOS Seatbelt chain is validated by the `cfg(macos)` test at the bottom
//! and by shoal-leash's own seatbelt profile tests; the mac CI job exercises
//! the live spawn.

use shoal_eval::Evaluator;
use shoal_leash::Policy;
use shoal_value::Value;
use std::path::{Path, PathBuf};

fn parse(src: &str) -> shoal_ast::Program {
    shoal_syntax::parse(src).expect("source parses")
}

/// An absolute path to a real external `cat`. Using an absolute head bypasses
/// shoal's in-process `cat` builtin, so the command genuinely spawns a child
/// and therefore actually travels through the sandbox activation path.
fn external_cat() -> PathBuf {
    for p in ["/bin/cat", "/usr/bin/cat"] {
        if Path::new(p).is_file() {
            return PathBuf::from(p);
        }
    }
    panic!("no external cat binary found for the spawn test");
}

fn cat_src(file: &Path) -> String {
    format!("{} {}", external_cat().display(), file.display())
}

/// Ensure the `shoal-sandbox-exec` launcher (a shoal-exec bin) is present beside
/// the test binary — cargo does not build a dependency's binaries for a
/// dependent crate's tests, so build it on demand when running `-p shoal-eval`
/// in isolation. Under a full workspace `cargo test` it is already there.
fn ensure_sandbox_helper() {
    let exe = std::env::current_exe().unwrap();
    let here = exe.parent().unwrap(); // .../deps
    let debug = here.parent().unwrap(); // .../debug
    if debug.join("shoal-sandbox-exec").is_file() || here.join("shoal-sandbox-exec").is_file() {
        return;
    }
    let target_dir = debug.parent().unwrap(); // .../target-xxx
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = std::process::Command::new(cargo)
        .args(["build", "-p", "shoal-exec", "--bin", "shoal-sandbox-exec"])
        .env("CARGO_TARGET_DIR", target_dir)
        .status()
        .expect("spawn cargo build for the sandbox helper");
    assert!(
        status.success(),
        "failed to build shoal-sandbox-exec helper"
    );
}

/// A scoped leash policy for `agent`: read only `allowed`, plus the system
/// directories a spawned `cat` needs to load and run — but NOT `secret`.
fn scoped_policy(allowed: &Path) -> Policy {
    let mut read = vec![format!("{}/**", allowed.display())];
    for sys in ["/usr", "/bin", "/lib", "/lib64", "/etc"] {
        if Path::new(sys).exists() {
            read.push(format!("{sys}/**"));
        }
    }
    let reads = read
        .iter()
        .map(|g| format!("\"{g}\""))
        .collect::<Vec<_>>()
        .join(", ");
    Policy::from_toml(&format!(
        "[principal.agent]\nopaque='allow'\n\n[principal.agent.fs]\nread=[{reads}]\n"
    ))
    .expect("scoped policy parses")
}

fn scene() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let allowed = d.path().join("allowed");
    let secret = d.path().join("secret");
    std::fs::create_dir(&allowed).unwrap();
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(allowed.join("ok.txt"), "OKDATA").unwrap();
    std::fs::write(secret.join("hidden.txt"), "SECRET").unwrap();
    (d, allowed, secret)
}

#[test]
fn permissive_policy_never_regresses_a_normal_command() {
    // The default-permissive policy must leave a normal spawn completely
    // unconfined: reading any file still works, exactly as with no policy.
    let (d, _allowed, secret) = scene();
    let mut ev = Evaluator::new(d.path().to_path_buf());
    ev.set_leash_policy(Policy::permissive("agent"), "agent");
    let src = cat_src(&secret.join("hidden.txt"));
    let out = ev
        .eval_program(&parse(&src))
        .expect("permissive policy leaves the child unconfined");
    let Value::Outcome(o) = out else {
        panic!("expected outcome, got {out:?}");
    };
    assert!(o.ok, "cat should succeed under a permissive policy");
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("SECRET"),
        "stdout was {:?}",
        String::from_utf8_lossy(&o.stdout)
    );
}

#[test]
fn no_leash_policy_is_the_unconfined_baseline() {
    // Sanity/control: with no policy at all a child reads the secret fine — so
    // the block in the next test is attributable to the sandbox, nothing else.
    let (d, _allowed, secret) = scene();
    let mut ev = Evaluator::new(d.path().to_path_buf());
    let src = cat_src(&secret.join("hidden.txt"));
    let out = ev.eval_program(&parse(&src)).expect("unconfined baseline");
    let Value::Outcome(o) = out else {
        panic!("expected outcome");
    };
    assert!(o.ok);
}

#[test]
fn scoped_policy_blocks_a_denied_sibling_through_the_eval_spawn_path() {
    if shoal_leash::landlock_abi().is_none() {
        eprintln!("Landlock unavailable; skipping the OS-enforcement assertion");
        return;
    }
    ensure_sandbox_helper();
    let (d, allowed, secret) = scene();
    let policy = scoped_policy(&allowed);

    // The granted file reads fine even under the scoped policy — proving the
    // sandbox is not just blanket-denying everything.
    {
        let mut ev = Evaluator::new(d.path().to_path_buf());
        ev.set_leash_policy(policy.clone(), "agent");
        let src = cat_src(&allowed.join("ok.txt"));
        let out = ev
            .eval_program(&parse(&src))
            .expect("granted path is readable under the scoped policy");
        let Value::Outcome(o) = out else {
            panic!("expected outcome");
        };
        assert!(
            o.ok && String::from_utf8_lossy(&o.stdout).contains("OKDATA"),
            "granted read failed: status={:?} stderr={:?}",
            o.status,
            String::from_utf8_lossy(&o.stderr)
        );
    }

    // The denied sibling cannot be read: cat gets EACCES from Landlock and
    // exits non-zero, which raises a `cmd_failed` error in statement position.
    {
        let mut ev = Evaluator::new(d.path().to_path_buf());
        ev.set_leash_policy(policy, "agent");
        let src = cat_src(&secret.join("hidden.txt"));
        let err = ev
            .eval_program(&parse(&src))
            .expect_err("denied sibling must not be readable under the scoped policy");
        assert_eq!(err.code, "cmd_failed", "unexpected error: {err:?}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_profile_chain_is_exercised_by_the_scoped_policy() {
    // On macOS the activation path routes through the Seatbelt backend. We can
    // at least prove the SandboxPolicy → Seatbelt profile step the spawn chain
    // relies on is reachable and deny-by-default for a scoped policy (the live
    // sandboxed spawn is validated by the mac CI job running the test above,
    // which is not Landlock-guarded on that platform).
    let (_d, allowed, _secret) = scene();
    let sandbox = scoped_policy(&allowed)
        .sandbox_for("agent")
        .expect("scoped policy yields a Seatbelt sandbox on macOS");
    let profile =
        shoal_leash::seatbelt_profile(&sandbox.fs).expect("seatbelt profile compiles for the fs");
    assert!(profile.starts_with("(version 1)\n(deny default)"));
    assert!(profile.contains("file-read*"));
}
