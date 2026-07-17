//! Interactive ergonomics coverage. See `site/content/internals/implementation-status.md`.
//!
//! Two dealbreakers, both fixed at the evaluator layer:
//!  1. statement-position builtins must render (their outcomes carry
//!     `streamed == false`, so the host renderer does NOT suppress them); only
//!     a `PtyTee` external — whose bytes actually hit the real terminal — is
//!     marked `streamed == true` and suppressed to avoid a double-print.
//!  2. `exit`/`quit` surfaces a code the host honors (via `take_exit`) instead
//!     of calling `std::process::exit` from inside eval.

use shoal_eval::Evaluator;
use shoal_value::Value;

fn parse(src: &str) -> shoal_ast::Program {
    shoal_syntax::parse(src).expect("fixture source parses")
}

/// A builtin in interactive statement position yields an outcome that was NOT
/// PtyTee-streamed, so the host must still render its `.out` (bug 1).
#[test]
fn builtin_outcome_is_not_streamed() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.set_interactive(true);
    let out = ev.eval_program(&parse("echo hello")).expect("echo runs");
    let Value::Outcome(o) = out else {
        panic!("echo should yield an outcome, got {out:?}");
    };
    assert!(
        !o.streamed,
        "a builtin streams nothing, so its outcome must render (streamed == false)"
    );
    assert_eq!(o.out_value(), Value::Str("hello".into()));
}

/// A captured (value-position) external also streams nothing → `streamed`
/// stays false so its `.out` renders like any other value.
#[test]
fn captured_external_is_not_streamed() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.set_interactive(true);
    // Value position (bound with `let`) forces Capture mode even interactively.
    let out = ev
        .eval_program(&parse("let r = (/usr/bin/printf hi); r"))
        .expect("capture runs");
    let Value::Outcome(o) = out else {
        panic!("expected a captured outcome, got {out:?}");
    };
    assert!(!o.streamed, "captured externals stream nothing");
    assert_eq!(String::from_utf8_lossy(&o.stdout), "hi");
}

/// An external in interactive statement position runs on a real PTY (PtyTee):
/// its bytes reach the terminal, so `streamed == true` and the host suppresses
/// re-rendering (prints exactly once).
#[test]
fn ptytee_external_is_streamed() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.set_interactive(true);
    let out = ev
        .eval_program(&parse("/usr/bin/true"))
        .expect("external runs");
    let Value::Outcome(o) = out else {
        panic!("expected an outcome, got {out:?}");
    };
    assert!(
        o.streamed,
        "a PtyTee external's bytes hit the tty, so it must be marked streamed"
    );
}

/// `exit <code>` surfaces the code via `take_exit`; eval never exits the
/// process (bug 2).
#[test]
fn exit_sets_pending_code() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.eval_program(&parse("exit 3")).expect("exit is a value");
    assert_eq!(ev.take_exit(), Some(3));
    // Consumed once.
    assert_eq!(ev.take_exit(), None);
}

/// Bare `exit` defaults to 0; `quit` is an alias.
#[test]
fn exit_defaults_zero_and_quit_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.eval_program(&parse("exit")).unwrap();
    assert_eq!(ev.take_exit(), Some(0));

    ev.eval_program(&parse("quit 2")).unwrap();
    assert_eq!(ev.take_exit(), Some(2));
}

/// `exit` halts the remaining statements in the program.
#[test]
fn exit_halts_remaining_statements() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.eval_program(&parse("let a = 1; exit 4; let b = 2"))
        .unwrap();
    assert_eq!(ev.take_exit(), Some(4));
    assert!(ev.env.get("a").is_some(), "statement before exit ran");
    assert!(
        ev.env.get("b").is_none(),
        "statement after exit must not run"
    );
}

/// A non-integer status is a clean `arg_error`, not a panic.
#[test]
fn exit_rejects_non_integer_status() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    let err = ev.eval_program(&parse("exit oops")).unwrap_err();
    assert_eq!(err.code, "arg_error");
    assert_eq!(ev.take_exit(), None, "a rejected exit sets no pending code");
}
