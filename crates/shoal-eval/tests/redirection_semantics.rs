//! Process redirection is a capture boundary even for an interactive session.
//! These regressions cover raw and adapter-backed commands without requiring a
//! real terminal: `Evaluator::set_interactive(true)` is enough to expose an
//! accidental PTY-mode selection through `Outcome.streamed`.

use shoal_adapters::AdapterCatalog;
use shoal_eval::Evaluator;
use shoal_value::Value;

fn outcome(value: Value) -> std::sync::Arc<shoal_value::OutcomeVal> {
    match value {
        Value::Outcome(outcome) => outcome,
        other => panic!("expected outcome, found {}", other.type_name()),
    }
}

#[test]
fn interactive_raw_output_redirect_forces_complete_capture() {
    let dir = tempfile::tempdir().unwrap();
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());
    evaluator.set_interactive(true);

    let value = evaluator
        .eval_program(&shoal_syntax::parse("printf redirected > raw.txt").unwrap())
        .unwrap();
    let outcome = outcome(value);

    assert!(
        !outcome.streamed,
        "redirected output must never use PTY tee"
    );
    assert_eq!(
        std::fs::read(dir.path().join("raw.txt")).unwrap(),
        b"redirected"
    );
}

#[test]
fn adapter_honors_input_and_output_redirects() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("input.txt"), b"adapter bytes\n").unwrap();
    std::fs::write(
        dir.path().join("fixture.toml"),
        r#"
[cmd.fixture]
bin = "cat"
class = "cli"
ok_codes = [0]
"#,
    )
    .unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
    assert!(warnings.is_empty(), "adapter warnings: {warnings:?}");

    let mut evaluator = Evaluator::new(dir.path().to_path_buf());
    evaluator.set_interactive(true);
    evaluator.set_adapters(catalog);
    let value = evaluator
        .eval_program(&shoal_syntax::parse("fixture < input.txt > output.txt").unwrap())
        .unwrap();
    let outcome = outcome(value);

    assert!(!outcome.streamed, "adapter redirects must force capture");
    assert_eq!(
        std::fs::read(dir.path().join("output.txt")).unwrap(),
        b"adapter bytes\n"
    );
}

#[test]
fn builtin_input_redirect_is_rejected_before_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());

    let error = evaluator
        .eval_program(&shoal_syntax::parse("touch victim < input.txt").unwrap())
        .expect_err("structured builtins do not own a stdin contract");

    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("does not consume redirected stdin"));
    assert!(
        !dir.path().join("victim").exists(),
        "redirect rejection happened after the builtin mutation"
    );
}

#[test]
fn invalid_builtin_output_target_is_rejected_before_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());

    let error = evaluator
        .eval_program(&shoal_syntax::parse("touch victim > (1 + 1)").unwrap())
        .expect_err("a redirect target must be a path before dispatch");

    assert_eq!(error.code, "arg_error");
    assert_eq!(error.msg, "redirect target must be a path");
    assert!(
        !dir.path().join("victim").exists(),
        "the builtin ran before its redirect target was admitted"
    );
}

#[test]
fn failed_statement_commits_redirect_before_raising() {
    let dir = tempfile::tempdir().unwrap();
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());
    evaluator.set_interactive(true);

    let error = evaluator
        .eval_program(&shoal_syntax::parse("^false > failed.txt").unwrap())
        .expect_err("false remains a statement error");

    assert_eq!(error.code, "cmd_failed");
    assert_eq!(error.status, Some(1));
    assert_eq!(std::fs::read(dir.path().join("failed.txt")).unwrap(), b"");
}

#[test]
fn failed_redirect_does_not_also_route_stdout_to_statement_sink() {
    use std::sync::{Arc, Mutex};

    let dir = tempfile::tempdir().unwrap();
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());
    let captured: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = captured.clone();
    evaluator.set_statement_sink(Box::new(move |value| {
        sink.lock().unwrap().push(value.clone());
    }));

    let error = evaluator
        .eval_program(
            &shoal_syntax::parse(
                "/bin/sh -c 'printf redirected-before-failure; exit 7' > failed.txt",
            )
            .unwrap(),
        )
        .expect_err("nonzero redirected statement must still raise");

    assert_eq!(error.code, "cmd_failed");
    assert_eq!(error.status, Some(7));
    assert_eq!(
        std::fs::read(dir.path().join("failed.txt")).unwrap(),
        b"redirected-before-failure"
    );
    assert!(
        captured.lock().unwrap().is_empty(),
        "redirected stdout must not also reach the statement sink"
    );
}
