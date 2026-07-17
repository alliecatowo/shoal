//! Child-evaluator context propagation (HR-B7, deep-audit finding B4).
//!
//! Every route that runs Shoal code in a fresh child evaluator derived from a
//! running session — `spawn { }`, `parallel(...)`, `on(channel, handler)`, and
//! a `.shl` script — MUST inherit the parent's active security/session context.
//! The audit found children silently dropped the leash policy/principal (and
//! reef/config), so a command a policy forbids foreground could quietly run
//! inside a `spawn`/`parallel`/handler/script.
//!
//! These tests configure a policy/config on the parent, then run the SAME
//! operation foreground and through each child route and assert the outcomes are
//! identical:
//!   * the runtime `proc_spawn` spawn-hash gate denies an unlisted binary in
//!     every route exactly as it does foreground (an in-process, pre-exec,
//!     platform-independent check — needs no sandbox helper or Landlock); and
//!   * an injected config snapshot is readable in every route.

use shoal_eval::Evaluator;
use shoal_leash::Policy;
use shoal_value::{ConfigSnapshot, Record, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn parse(src: &str) -> shoal_ast::Program {
    shoal_syntax::parse(src).expect("source parses")
}

/// An absolute path to a real external `cat`. An absolute head bypasses shoal's
/// in-process `cat` builtin, so the command genuinely spawns a child and
/// therefore actually travels through the spawn gate.
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

/// A principal whose `proc_spawn` allowlist contains only `entry`, with
/// `opaque='allow'` so nothing else interferes. `cat` is not listed, so the
/// spawn gate denies it before exec with `spawn_denied`.
fn spawn_pinned_policy(entry: &str) -> Policy {
    Policy::from_toml(&format!(
        "[principal.agent]\nopaque='allow'\nproc_spawn = [\"{entry}\"]\n"
    ))
    .expect("spawn-pinned policy parses")
}

fn scene() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let ok = d.path().join("ok.txt");
    std::fs::write(&ok, "OKDATA").unwrap();
    (d, ok)
}

/// A fresh evaluator rooted at `cwd` under a restrictive spawn-pin policy that
/// forbids `cat`.
fn restricted_evaluator(cwd: &Path) -> Evaluator {
    let mut ev = Evaluator::new(cwd.to_path_buf());
    ev.set_leash_policy(spawn_pinned_policy("some-tool-that-is-not-cat"), "agent");
    ev
}

/// A fresh evaluator rooted at `cwd` with an injected config snapshot carrying
/// `b_marker = "propagated"`.
fn config_evaluator(cwd: &Path) -> Evaluator {
    let mut ev = Evaluator::new(cwd.to_path_buf());
    let mut rec = Record::new();
    rec.insert("b_marker".into(), Value::Str("propagated".into()));
    ev.set_config(Arc::new(ConfigSnapshot::new(Value::Record(rec))));
    ev
}

// ---- Leash spawn-hash gate: identical denial across every route -------------

#[test]
fn foreground_spawn_gate_denies_unlisted_cat() {
    // The baseline the child routes must match: a restricted principal's spawn
    // of `cat` is denied before exec.
    let (d, ok) = scene();
    let mut ev = restricted_evaluator(d.path());
    let err = ev
        .eval_program(&parse(&cat_src(&ok)))
        .expect_err("foreground spawn of an unlisted binary must be denied");
    assert_eq!(err.code, "spawn_denied", "foreground: {err:?}");
}

#[test]
fn spawn_block_inherits_the_spawn_gate() {
    // The SAME command inside `spawn { }` must be denied identically — the
    // child inherits the parent's leash policy/principal.
    let (d, ok) = scene();
    let mut ev = restricted_evaluator(d.path());
    let src = format!("(spawn {{ {} }}).await()", cat_src(&ok));
    let err = ev
        .eval_program(&parse(&src))
        .expect_err("a spawn child must inherit the spawn-gate denial");
    assert_eq!(err.code, "spawn_denied", "spawn: {err:?}");
}

#[test]
fn parallel_inherits_the_spawn_gate() {
    // `parallel` is fail-fast: a denied child surfaces its `spawn_denied` as the
    // call's error.
    let (d, ok) = scene();
    let mut ev = restricted_evaluator(d.path());
    let src = format!("parallel(() => {{ {} }})", cat_src(&ok));
    let err = ev
        .eval_program(&parse(&src))
        .expect_err("a parallel child must inherit the spawn-gate denial");
    assert_eq!(err.code, "spawn_denied", "parallel: {err:?}");
}

#[test]
fn on_handler_inherits_the_spawn_gate() {
    // An `on(channel, handler)` task runs its handler in a child evaluator. The
    // handler catches the command result so the parent can observe it
    // deterministically over a channel (no reliance on an endless stream ending):
    // `"DENIED"` means the spawn gate fired inside the handler; `true` (the
    // outcome's `.ok`) would mean the child ran unconfined.
    let (d, ok) = scene();
    let mut ev = restricted_evaluator(d.path());
    let src = format!(
        "on(channel(\"cmd\"), (ev) => {{ channel(\"res\").emit(try {{ ({}).ok }} catch {{ \"DENIED\" }}) }})\n\
         channel(\"cmd\").emit(1)\n\
         channel(\"res\").take(timeout: 5s)",
        cat_src(&ok)
    );
    let out = ev
        .eval_program(&parse(&src))
        .expect("handler reports a result");
    assert_eq!(
        out,
        Value::Str("DENIED".into()),
        "an on-handler child must inherit the spawn-gate denial, got {out:?}"
    );
}

#[test]
fn shl_script_inherits_the_spawn_gate() {
    // A `.shl` script run via `run(path)` is a separate program in a child
    // evaluator; its spawn must be denied identically to foreground.
    let (d, ok) = scene();
    std::fs::write(d.path().join("s.shl"), cat_src(&ok)).unwrap();
    let mut ev = restricted_evaluator(d.path());
    let err = ev
        .eval_program(&parse("run(\"s.shl\")"))
        .expect_err("a .shl child must inherit the spawn-gate denial");
    assert_eq!(err.code, "spawn_denied", "script: {err:?}");
}

// ---- Config snapshot: identical reads across every route --------------------

#[test]
fn foreground_reads_injected_config() {
    let (d, _ok) = scene();
    let mut ev = config_evaluator(d.path());
    let out = ev
        .eval_program(&parse("config.get(\"b_marker\")"))
        .expect("config read");
    assert_eq!(out, Value::Str("propagated".into()), "foreground: {out:?}");
}

#[test]
fn spawn_block_inherits_config() {
    let (d, _ok) = scene();
    let mut ev = config_evaluator(d.path());
    let out = ev
        .eval_program(&parse("(spawn { config.get(\"b_marker\") }).await()"))
        .expect("spawn config read");
    assert_eq!(out, Value::Str("propagated".into()), "spawn: {out:?}");
}

#[test]
fn parallel_inherits_config() {
    let (d, _ok) = scene();
    let mut ev = config_evaluator(d.path());
    let out = ev
        .eval_program(&parse("parallel(() => config.get(\"b_marker\"))"))
        .expect("parallel config read");
    assert_eq!(
        out,
        Value::List(vec![Value::Str("propagated".into())]),
        "parallel: {out:?}"
    );
}

#[test]
fn on_handler_inherits_config() {
    let (d, _ok) = scene();
    let mut ev = config_evaluator(d.path());
    let src = "on(channel(\"cmd\"), (ev) => { channel(\"cfg\").emit(config.get(\"b_marker\")) })\n\
               channel(\"cmd\").emit(1)\n\
               channel(\"cfg\").take(timeout: 5s)";
    let out = ev.eval_program(&parse(src)).expect("handler config read");
    assert_eq!(out, Value::Str("propagated".into()), "on: {out:?}");
}

#[test]
fn shl_script_inherits_config() {
    let (d, _ok) = scene();
    std::fs::write(d.path().join("cfg.shl"), "config.get(\"b_marker\")").unwrap();
    let mut ev = config_evaluator(d.path());
    let out = ev
        .eval_program(&parse("run(\"cfg.shl\")"))
        .expect("script config read");
    assert_eq!(out, Value::Str("propagated".into()), "script: {out:?}");
}

// ---- Cancellation: parent cancellation reaches synchronous children ---------
//
// `spawn`/`on` wire a FRESH token to their task's cancel hook (task.cancel()
// interrupts them). The two synchronous routes — a `.shl` script and a
// `parallel` batch — instead inherit the PARENT'S cancellation token, so a host
// cancel reaches them. Pre-cancelling the parent deterministically proves the
// linkage: `sleep` polls the token and returns promptly when cancelled, so an
// inheriting child aborts a long sleep immediately, while an unlinked child (the
// pre-fix behavior) would sleep the full duration.

#[test]
fn parallel_children_observe_parent_cancellation() {
    let (d, _ok) = scene();
    let mut ev = Evaluator::new(d.path().to_path_buf());
    ev.cancel_current(); // cancel the parent BEFORE running
    let start = Instant::now();
    // The child closure sleeps 30s then yields 1; if it inherited the parent's
    // (cancelled) token the sleep returns at once and the batch completes fast.
    let out = ev
        .eval_program(&parse("parallel(() => { sleep 30\n1 })"))
        .expect("parallel returns");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "a parallel child must observe the parent's cancellation and abort its sleep, took {:?}",
        start.elapsed()
    );
    assert_eq!(out, Value::List(vec![Value::Int(1)]), "parallel: {out:?}");
}

#[test]
fn shl_script_child_observes_parent_cancellation() {
    let (d, _ok) = scene();
    std::fs::write(d.path().join("sleeper.shl"), "sleep 30\n7").unwrap();
    let mut ev = Evaluator::new(d.path().to_path_buf());
    ev.cancel_current(); // cancel the parent BEFORE running
    let start = Instant::now();
    let out = ev
        .eval_program(&parse("run(\"sleeper.shl\")"))
        .expect("script runs");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "a .shl script child must observe the parent's cancellation, took {:?}",
        start.elapsed()
    );
    assert_eq!(out, Value::Int(7), "script: {out:?}");
}
