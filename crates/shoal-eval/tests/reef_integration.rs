//! reef integration tests (docs/REEF.md §1–§6).
//!
//! Every test builds a self-contained tempdir project — a `.reef.toml` plus
//! fixture "binaries" (shell scripts with a `--version`) — and points the reef
//! resolver at those fixtures. Nothing touches real `~/.config` or the network.
//! The zero-regression tests prove a NO-manifest tempdir spawns exactly as
//! before.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use shoal_eval::Evaluator;
use shoal_reef::Resolver;
use shoal_reef::provider::SystemProvider;
use shoal_value::Value;

/// Write an executable fixture "binary": a shell script that answers
/// `--version` and otherwise prints a fixed marker line.
fn fixture_bin(dir: &Path, name: &str, version: &str) -> PathBuf {
    let p = dir.join(name);
    let body = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{name} {version}\"; exit 0; fi\necho \"{name}-ran\"\n"
    );
    std::fs::write(&p, body).unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&p, perm).unwrap();
    p
}

/// A resolver whose only provider is a system provider rooted at `bindir`, so
/// fixture tools resolve without touching the real system PATH.
fn fixture_resolver(bindir: &Path) -> Arc<Resolver> {
    Arc::new(Resolver::new(vec![Box::new(SystemProvider::new(
        vec![bindir.to_path_buf()],
        vec![],
    ))]))
}

fn parse(src: &str) -> shoal_ast::Program {
    shoal_syntax::parse(src).expect("fixture source parses")
}

/// Build a project dir with a `.reef.toml` and a `bin/` of fixtures.
fn project(reef_toml: &str, fixtures: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".reef.toml"), reef_toml).unwrap();
    let bindir = dir.path().join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    for (name, ver) in fixtures {
        fixture_bin(&bindir, name, ver);
    }
    (dir, bindir)
}

#[test]
fn constrained_tool_resolves_to_fixture_and_spawns() {
    let (dir, bindir) = project("[tools]\nfaketool = \"*\"\n", &[("faketool", "1.2.3")]);
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.interactive = true; // interactive → auto-lock, no reef_unlocked
    ev.set_reef_resolver(fixture_resolver(&bindir));

    let out = ev.eval_program(&parse("faketool")).expect("faketool runs");
    let Value::Outcome(o) = out else {
        panic!("expected an outcome, got {out:?}");
    };
    assert!(o.ok, "fixture exits 0");
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(stdout.contains("faketool-ran"), "stdout was {stdout:?}");
    // argv[0] was rewritten to the resolved absolute fixture path.
    assert!(
        o.cmd
            .contains(bindir.join("faketool").to_string_lossy().as_ref()),
        "resolved cmd was {:?}",
        o.cmd
    );
    // Interactive auto-lock wrote reef.lock next to the manifest.
    assert!(dir.path().join("reef.lock").exists(), "auto-lock persisted");
}

#[test]
fn which_shows_the_resolution_chain() {
    let (dir, bindir) = project("[tools]\nfaketool = \"*\"\n", &[("faketool", "1.2.3")]);
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.interactive = true;
    ev.set_reef_resolver(fixture_resolver(&bindir));

    let out = ev
        .eval_program(&parse("which faketool"))
        .expect("which runs");
    let Value::Outcome(o) = out else {
        panic!("expected outcome");
    };
    let Some(Value::Record(r)) = o.parsed.as_ref().cloned() else {
        panic!("which should carry a record report, got {:?}", o.parsed);
    };
    assert_eq!(r.get("name"), Some(&Value::Str("faketool".into())));
    assert_eq!(r.get("scope"), Some(&Value::Str("reef".into())));
    match r.get("path") {
        Some(Value::Path(p)) => assert_eq!(p, &bindir.join("faketool")),
        other => panic!("path field was {other:?}"),
    }
    // The chain records the reef scope's decision.
    match r.get("chain") {
        Some(Value::Table(rows)) => {
            assert!(!rows.is_empty(), "chain should list the scope decisions");
            assert!(
                rows.iter()
                    .any(|row| row.get("outcome") == Some(&Value::Str("selected".into()))),
                "the winning scope is marked selected"
            );
        }
        other => panic!("chain field was {other:?}"),
    }
}

#[test]
fn script_mode_unlocked_constraint_errors() {
    let (dir, bindir) = project("[tools]\nfaketool = \"*\"\n", &[("faketool", "1.2.3")]);
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.interactive = false; // script/CI policy → hard error on unlocked constraint
    ev.set_reef_resolver(fixture_resolver(&bindir));

    let err = ev
        .eval_program(&parse("faketool"))
        .expect_err("script mode must not guess an unlocked tool");
    assert_eq!(err.code, "reef_unlocked", "got {} / {}", err.code, err.msg);
    assert!(err.span.is_some(), "error carries the head's span");
}

#[test]
fn constrained_but_missing_tool_reports_did_you_mean() {
    // Manifest constrains `ghosttool`, but no fixture provides it.
    let (dir, bindir) = project("[tools]\nghosttool = \"9\"\n", &[]);
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.interactive = true;
    ev.set_reef_resolver(fixture_resolver(&bindir));

    let err = ev
        .eval_program(&parse("ghosttool"))
        .expect_err("missing constrained tool errors");
    assert_eq!(err.code, "reef_not_found");
    assert!(
        err.msg.contains("constrained") && err.msg.contains("reef fetch ghosttool"),
        "did-you-mean phrasing, got {:?}",
        err.msg
    );
}

#[test]
fn reef_builtin_lists_bindings() {
    let (dir, bindir) = project("[tools]\nfaketool = \"*\"\n", &[("faketool", "1.2.3")]);
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.interactive = true;
    ev.set_reef_resolver(fixture_resolver(&bindir));

    let out = ev.eval_program(&parse("reef")).expect("reef runs");
    let Value::Outcome(o) = out else {
        panic!("expected outcome");
    };
    let Some(Value::Table(rows)) = o.parsed.as_ref().cloned() else {
        panic!("reef should carry a table, got {:?}", o.parsed);
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("name"), Some(&Value::Str("faketool".into())));
    assert_eq!(rows[0].get("scope"), Some(&Value::Str("reef".into())));
}

#[test]
fn reef_add_writes_manifest_and_locks() {
    // Start with a project that has a manifest for one tool; add a second.
    let (dir, bindir) = project(
        "[tools]\nfaketool = \"*\"\n",
        &[("faketool", "1"), ("other", "2")],
    );
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.interactive = true;
    ev.set_reef_resolver(fixture_resolver(&bindir));

    let out = ev
        .eval_program(&parse("reef add other@2"))
        .expect("reef add runs");
    let Value::Outcome(o) = out else {
        panic!("expected outcome");
    };
    let Some(Value::Record(r)) = o.parsed.as_ref().cloned() else {
        panic!("reef add should carry a record, got {:?}", o.parsed);
    };
    assert_eq!(r.get("locked"), Some(&Value::Bool(true)));
    // The manifest now mentions `other`.
    let manifest = std::fs::read_to_string(dir.path().join(".reef.toml")).unwrap();
    assert!(manifest.contains("other"), "manifest was {manifest:?}");
    // And the lock has an entry for it.
    let lock = std::fs::read_to_string(dir.path().join("reef.lock")).unwrap();
    assert!(lock.contains("other"), "lock was {lock:?}");
}

// --- zero-regression: NO manifest behaves exactly as before ----------------

#[test]
fn no_manifest_spawns_via_path_exactly_as_before() {
    // A tempdir with NO .reef.toml, NO user config. The reef path must fast-bail
    // to today's PATH resolution — a plain command runs via `PATH` and succeeds,
    // no lock is written.
    let dir = tempfile::tempdir().unwrap();
    let bindir = dir.path().join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    fixture_bin(&bindir, "mytool", "1");
    let mut ev = Evaluator::new(dir.path().to_path_buf());

    let src = format!("PATH={} mytool", bindir.display());
    let out = ev.eval_program(&parse(&src)).expect("mytool runs via PATH");
    let Value::Outcome(o) = out else {
        panic!("expected outcome, got {out:?}");
    };
    assert!(o.ok, "the command exits 0 exactly as today");
    assert!(o.pid != 0, "a real child was spawned");
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("mytool-ran"),
        "ran the PATH-resolved binary"
    );
    // No reef side effects anywhere.
    assert!(!dir.path().join("reef.lock").exists());
}

#[test]
fn no_manifest_which_returns_ambient_path_entry() {
    // `which sh` with no manifest must still find the ambient binary (a minimal
    // report), never Null — no regression from today's PATH lookup.
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    let out = ev.eval_program(&parse("which sh")).expect("which sh runs");
    let Value::Outcome(o) = out else {
        panic!("expected outcome");
    };
    match o.parsed.as_ref() {
        Some(Value::Record(r)) => match r.get("path") {
            Some(Value::Path(p)) => assert!(
                p.file_name().is_some_and(|n| n == "sh"),
                "resolved a real sh, got {p:?}"
            ),
            other => panic!("path field was {other:?}"),
        },
        other => panic!("which should return a record, got {other:?}"),
    }
}

#[test]
fn unmentioned_tool_is_passthrough_even_under_script_policy() {
    // Manifest constrains `faketool`, but a spawn of the UNMENTIONED `othertool`
    // must be pure passthrough: no reef_unlocked error under script policy, and
    // it resolves via ambient PATH exactly as today.
    let (dir, bindir) = project("[tools]\nfaketool = \"*\"\n", &[("faketool", "1")]);
    fixture_bin(&bindir, "othertool", "9"); // present, but NOT in the manifest
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.interactive = false; // script policy — a constrained miss would error
    ev.set_reef_resolver(fixture_resolver(&bindir));

    let src = format!("PATH={} othertool", bindir.display());
    let out = ev
        .eval_program(&parse(&src))
        .expect("unmentioned tool is passthrough, not a reef error");
    let Value::Outcome(o) = out else {
        panic!("expected outcome, got {out:?}");
    };
    assert!(o.ok, "unmentioned `othertool` runs via ambient PATH");
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("othertool-ran"),
        "ran the PATH-resolved binary, not a reef-locked one"
    );
    // The reef resolver was never engaged for it, so no lock entry was written.
    let lock = std::fs::read_to_string(dir.path().join("reef.lock")).unwrap_or_default();
    assert!(
        !lock.contains("othertool"),
        "unmentioned tool must not be locked"
    );
}
