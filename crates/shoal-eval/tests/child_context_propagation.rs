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
use shoal_reef::Resolver;
use shoal_reef::provider::SystemProvider;
use shoal_value::{ConfigSnapshot, Record, Value};
use std::os::unix::fs::PermissionsExt;
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
         channel(\"res\").events(since: 0).take(1).take_until(every(30s)).map(ev => ev.payload).collect().first()",
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
               channel(\"cfg\").events(since: 0).take(1).take_until(every(30s)).map(ev => ev.payload).collect().first()";
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

// ---- Reef resolution inputs: a constrained tool resolves in every route -----
//
// The reef-half of finding B (the parent's reef resolver, scope chain, lock, and
// lock path). A parent whose `.reef.toml` constrains `faketool` to a fixture
// binary LOCKS it (an interactive foreground run auto-locks). Each child route
// must inherit those reef inputs so the SAME constrained name resolves to the
// SAME fixture binary from the inherited lock under the child's non-interactive
// script policy. A child that dropped the reef inputs would not resolve
// `faketool` at all — the fixture dir is not on the process `PATH`. This pins
// the step-3 `ReefState` bundle (resolver + chain + lock + lock path)
// behaviorally, complementing the white-box overlay pin in `child_context.rs`.

/// Write an executable fixture `faketool` that answers `--version` and otherwise
/// prints a fixed marker line, into `bindir`.
fn fixture_faketool(bindir: &Path) {
    let p = bindir.join("faketool");
    std::fs::write(
        &p,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"faketool 1.2.3\"; exit 0; fi\necho \"faketool-ran\"\n",
    )
    .unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&p, perm).unwrap();
}

/// A parent rooted in a fresh project whose `.reef.toml` constrains `faketool`,
/// with a fixture resolver rooted at the project `bin/`, and `faketool` already
/// LOCKED (an interactive foreground run auto-locks) so a child resolves it from
/// the inherited lock under the non-interactive script policy.
fn reef_locked_parent() -> (tempfile::TempDir, Evaluator) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".reef.toml"), "[tools]\nfaketool = \"*\"\n").unwrap();
    let bindir = dir.path().join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    fixture_faketool(&bindir);
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.set_interactive(true); // interactive → auto-lock, no reef_unlocked
    ev.set_reef_resolver(Arc::new(Resolver::new(vec![Box::new(
        SystemProvider::new(vec![bindir], vec![]),
    )])));
    ev.eval_program(&parse("faketool"))
        .expect("parent locks faketool foreground");
    (dir, ev)
}

/// Whether `v` is the successful outcome of the resolved fixture binary.
fn ran_fixture(v: &Value) -> bool {
    matches!(v, Value::Outcome(o)
        if o.ok && String::from_utf8_lossy(&o.stdout).contains("faketool-ran"))
}

#[test]
fn spawn_block_inherits_reef_resolution() {
    let (_d, mut ev) = reef_locked_parent();
    let out = ev
        .eval_program(&parse("(spawn { faketool }).await()"))
        .expect("spawn faketool runs");
    assert!(
        ran_fixture(&out),
        "a spawn child must resolve the constrained fixture, got {out:?}"
    );
}

#[test]
fn parallel_inherits_reef_resolution() {
    let (_d, mut ev) = reef_locked_parent();
    // A bare command in a lambda body needs a block so `faketool` is a command,
    // not a variable read (an expression-position identifier).
    let out = ev
        .eval_program(&parse("parallel(() => { faketool })"))
        .expect("parallel faketool runs");
    let Value::List(xs) = &out else {
        panic!("parallel returns a list, got {out:?}");
    };
    assert!(
        xs.len() == 1 && ran_fixture(&xs[0]),
        "a parallel child must resolve the constrained fixture, got {out:?}"
    );
}

#[test]
fn on_handler_inherits_reef_resolution() {
    let (_d, mut ev) = reef_locked_parent();
    // The handler reports a deterministic result over a channel (as the leash
    // on-handler test does): `true` (the outcome's `.ok`) means `faketool`
    // resolved to the fixture inside the handler's child; a child that dropped
    // the reef inputs could not resolve it, so `(faketool).ok` raises and the
    // handler reports `"MISS"`.
    let src = "on(channel(\"cmd\"), (ev) => { channel(\"res\").emit(try { (faketool).ok } catch { \"MISS\" }) })\n\
               channel(\"cmd\").emit(1)\n\
               channel(\"res\").events(since: 0).take(1).take_until(every(30s)).map(ev => ev.payload).collect().first()";
    let out = ev
        .eval_program(&parse(src))
        .expect("on-handler reports a result");
    assert_eq!(
        out,
        Value::Bool(true),
        "an on-handler child must resolve the constrained fixture, got {out:?}"
    );
}

#[test]
fn shl_script_inherits_reef_resolution() {
    let (d, mut ev) = reef_locked_parent();
    std::fs::write(d.path().join("r.shl"), "faketool").unwrap();
    let out = ev
        .eval_program(&parse("run(\"r.shl\")"))
        .expect("script faketool runs");
    assert!(
        ran_fixture(&out),
        "a .shl child must resolve the constrained fixture, got {out:?}"
    );
}

// ---- Event bus is shared across the child boundary --------------------------
//
// Session `channel(name)` coordination is one shared `Arc<EventBus>`. That the
// child inherits the SAME `Arc` (not a private bus) is pinned white-box by
// `Arc::ptr_eq` in `child_context.rs::decomposition_characterization`; the
// `on`-handler routes above additionally exercise a child publishing onto a
// channel the parent consumes. Those assertions use the bounded channel ring's
// `since: 0` replay, so a fast child cannot publish between the trigger and a
// late `.take()` subscription and turn context propagation into a scheduler
// race. A 30-second timer keeps a real missing publication bounded.
