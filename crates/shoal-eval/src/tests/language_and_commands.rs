use super::*;

#[test]
fn defect1_nonfinal_and_block_commands_reach_sink() {
    // Non-final top-level statement values pass through to the sink; the
    // final value is returned. Every command now yields an outcome whose
    // `.out` carries the joined echo text (outcome unification, P1a).
    let (out, captured) = run_capturing("echo hi\necho bye");
    assert_eq!(out_of(&out.unwrap()), Value::Str("bye".into()));
    assert_eq!(captured.len(), 1);
    assert_eq!(out_of(&captured[0]), Value::Str("hi".into()));

    // Every iteration of a loop body's bare command reaches the sink.
    let (_out, captured) = run_capturing("for x in [1,2,3] { echo (x) }");
    let texts: Vec<Value> = captured.iter().map(out_of).collect();
    assert_eq!(
        texts,
        vec![
            Value::Str("1".into()),
            Value::Str("2".into()),
            Value::Str("3".into()),
        ]
    );
}

#[test]
fn loop_assignment_branches_do_not_leak_values_to_output() {
    let (out, captured) =
        run_capturing("var total = 0\nfor n in [1, 2, 3] { if n > 0 { total += n } }\ntotal");
    assert_eq!(out.unwrap(), Value::Int(6));
    assert!(captured.is_empty(), "assigned values leaked: {captured:?}");

    let (_out, captured) = run_capturing("for n in [1, 2] { if n > 0 { echo (n) } }");
    assert_eq!(
        captured.iter().map(out_of).collect::<Vec<_>>(),
        vec![Value::Str("1".into()), Value::Str("2".into()),]
    );
}

/// `render.echo` (site/content/internals/configuration-reference.md): [`EchoMode`] gates which non-final
/// top-level statement values route to the statement sink. `Quiet`/
/// `Commands` suppress intermediate pure expressions (`1+1`) but still echo
/// intermediate bare commands; `All` (the default) echoes every
/// intermediate. The final value is always returned to the host, never sunk.
#[test]
fn echo_mode_gates_intermediate_statement_rendering() {
    use std::sync::{Arc, Mutex};
    let run_mode = |src: &str, mode: EchoMode| -> (VResult<Value>, Vec<Value>) {
        let program = shoal_syntax::parse(src).unwrap();
        let mut ev = Evaluator::new(std::env::current_dir().unwrap());
        ev.set_echo_mode(mode);
        let sink: Arc<Mutex<Vec<Value>>> = Arc::default();
        let sink2 = sink.clone();
        ev.set_statement_sink(Box::new(move |v: &Value| {
            sink2.lock().unwrap().push(v.clone())
        }));
        let out = ev.eval_program(&program);
        drop(ev);
        (out, Arc::try_unwrap(sink).unwrap().into_inner().unwrap())
    };
    let sunk = |captured: &[Value]| captured.iter().map(out_of).collect::<Vec<_>>();

    // Quiet: the intermediate `1+1` is NOT sunk; the intermediate `echo hi`
    // (a bare command) still is; the final `42` is returned, never sunk.
    let (out, captured) = run_mode("1+1\necho hi\n42", EchoMode::Quiet);
    assert_eq!(out.unwrap(), Value::Int(42));
    assert_eq!(sunk(&captured), vec![Value::Str("hi".into())]);

    // Commands: same intermediate gate as Quiet (only bare commands echo).
    let (out, captured) = run_mode("1+1\necho hi\n42", EchoMode::Commands);
    assert_eq!(out.unwrap(), Value::Int(42));
    assert_eq!(sunk(&captured), vec![Value::Str("hi".into())]);

    // All (the default): every intermediate is sunk — the `1+1` too.
    let (out, captured) = run_mode("1+1\necho hi\n42", EchoMode::All);
    assert_eq!(out.unwrap(), Value::Int(42));
    assert_eq!(
        sunk(&captured),
        vec![Value::Int(2), Value::Str("hi".into())]
    );
}

/// Decision 2: the in-language `config` namespace reads the host-INJECTED
/// snapshot (`set_config`), never a `shoal.toml` walked off the filesystem.
/// So an injected snapshot wins over an on-disk file, and with NO snapshot
/// the answer is `null` (no filesystem fallback) — the kernel-less/test
/// default that keeps `config.get` for an unset key behaving as before.
#[test]
fn config_namespace_reads_injected_snapshot_not_the_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    // An on-disk shoal.toml the OLD filesystem-walking implementation would
    // have read; it must be ignored by both paths below.
    std::fs::write(dir.path().join("shoal.toml"), "greeting = \"from-disk\"\n").unwrap();

    let get = shoal_syntax::parse("config.get(\"greeting\")").unwrap();

    // Injected snapshot → `config.get` reads THAT, not the on-disk file.
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    let mut rec = Record::new();
    rec.insert("greeting".into(), Value::Str("from-snapshot".into()));
    ev.set_config(Arc::new(ConfigSnapshot::new(Value::Record(rec))));
    assert_eq!(
        ev.eval_program(&get).unwrap(),
        Value::Str("from-snapshot".into())
    );

    // No snapshot injected → degrades to null; does NOT fall back to the
    // on-disk shoal.toml sitting in the cwd.
    let mut ev2 = Evaluator::new(dir.path().to_path_buf());
    assert_eq!(ev2.eval_program(&get).unwrap(), Value::Null);

    // `config.all()` returns the whole injected snapshot record.
    let all = shoal_syntax::parse("config.all()").unwrap();
    let mut ev3 = Evaluator::new(dir.path().to_path_buf());
    let mut rec = Record::new();
    rec.insert("k".into(), Value::Int(7));
    ev3.set_config(Arc::new(ConfigSnapshot::new(Value::Record(rec.clone()))));
    assert_eq!(ev3.eval_program(&all).unwrap(), Value::Record(rec));
}

#[test]
fn outcome_unification_builtin_out_and_ok() {
    // A builtin is an outcome: `.out` is its structured result, `.ok` true.
    assert_eq!(run("(echo hi).out").unwrap(), Value::Str("hi".into()));
    assert_eq!(run("(echo hi).ok").unwrap(), Value::Bool(true));
    // Unknown fields forward to `.out` (stat record → `.size`).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), b"xyz").unwrap();
    assert_eq!(run_in("(stat a).size", dir.path()).unwrap(), Value::Size(3));
}

#[test]
fn outcome_unification_and_or_compose_commands() {
    // `echo a && echo b` prints BOTH (P1d): `a` via the sink, `b` returned.
    let (out, captured) = run_capturing("echo a && echo b");
    assert_eq!(out_of(&out.unwrap()), Value::Str("b".into()));
    assert_eq!(
        captured.iter().map(out_of).collect::<Vec<_>>(),
        vec![Value::Str("a".into())]
    );
    // A three-stage chain prints every stage.
    let (out, captured) = run_capturing("echo a && echo b && echo c");
    assert_eq!(out_of(&out.unwrap()), Value::Str("c".into()));
    assert_eq!(
        captured.iter().map(out_of).collect::<Vec<_>>(),
        vec![Value::Str("a".into()), Value::Str("b".into())]
    );
    // `||` recovers from a failed command without raising.
    let out = run("sh { exit 1 } || echo x").unwrap();
    assert_eq!(out_of(&out), Value::Str("x".into()));
}

#[test]
fn outcome_forwards_collection_methods() {
    // `ls` is an outcome; `.where`/`.sort`/`.first(n)`/`.map` forward to its
    // `.out` table (outcome unification P1b + first(n) arity fix).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big"), vec![0u8; 2048]).unwrap();
    std::fs::write(dir.path().join("small"), b"x").unwrap();
    let names = run_in("ls.where(.size > 1b).sort(.name).map(.name)", dir.path()).unwrap();
    assert_eq!(names, Value::List(vec![Value::Path("big".into())]));
    // `.first(2)` returns a LIST of two, chainable into `.map`.
    std::fs::write(dir.path().join("mid"), vec![0u8; 4]).unwrap();
    let first_two = run_in("ls.sort(.name).first(2).map(.name)", dir.path()).unwrap();
    assert!(matches!(first_two, Value::List(xs) if xs.len() == 2));
}

#[test]
fn double_echo_fixed_and_bare_echo_blank_line() {
    // A fn whose last body statement is a bare command prints ONCE: the
    // trailing command is the block value, not also sunk.
    let (out, captured) = run_capturing("fn g(){ echo hi }\ng()");
    assert_eq!(out_of(&out.unwrap()), Value::Str("hi".into()));
    assert!(
        captured.is_empty(),
        "trailing command must not double-print: {captured:?}"
    );
    // Bare `echo` emits a blank line: its outcome stdout is "\n".
    let (_out, captured) = run_capturing("echo\n42");
    assert_eq!(captured.len(), 1);
    match &captured[0] {
        Value::Outcome(o) => assert_eq!(&*o.stdout, b"\n"),
        other => panic!("expected outcome, got {other:?}"),
    }
}

#[test]
fn top_level_ls_renders_as_table() {
    // An outcome with a structured `.out` renders as that structure (P1c).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("only"), b"x").unwrap();
    let v = run_in("ls", dir.path()).unwrap();
    let rendered = shoal_value::render::render_block(&v, 80);
    assert!(
        rendered.contains("name"),
        "ls should render a table: {rendered:?}"
    );
    assert!(
        rendered.contains("only"),
        "ls table should list the file: {rendered:?}"
    );
}

#[test]
fn defect3_forced_command_still_resolves_session_fn() {
    assert_eq!(
        run("fn greet(n:str){ (n) }\n^greet world").unwrap(),
        Value::Str("world".into())
    );
}

#[test]
fn defect4_stat_modified_is_datetime() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), b"x").unwrap();
    let v = run_in("stat a", dir.path()).unwrap();
    let Value::Record(r) = out_of(&v) else {
        panic!("stat should be a record")
    };
    assert!(
        matches!(r.get("modified"), Some(Value::DateTime(_))),
        "modified must be a DateTime, got {:?}",
        r.get("modified")
    );
}

#[test]
fn defect5_command_resolves_in_value_position() {
    // `let r = ls` invokes the builtin zero-arg in value position.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), b"x").unwrap();
    let v = run_in("let r = ls\nr", dir.path()).unwrap();
    // `ls` now yields an outcome; its `.out` is the table (P1a).
    assert!(matches!(out_of(&v), Value::Table(rows) if rows.len() == 1));
}

#[test]
fn defect5_env_field_read_via_command() {
    // `env.PATH` reads by invoking the `env` builtin then projecting.
    unsafe { std::env::set_var("SHOAL_TEST_VAR", "hello") };
    let v = run("env.SHOAL_TEST_VAR").unwrap();
    assert_eq!(v, Value::Str("hello".into()));
}

#[test]
fn defect8_redirect_applies_to_builtin() {
    let dir = tempfile::tempdir().unwrap();
    run_in("echo hi > b.txt", dir.path()).unwrap();
    let body = std::fs::read_to_string(dir.path().join("b.txt")).unwrap();
    assert_eq!(body, "hi\n");
}

#[test]
fn defect9_recursion_guard_returns_error() {
    let (at_limit, code, message) = std::thread::Builder::new()
        // Debug evaluator frames are materially larger on macOS than Linux.
        // Keep the test stack explicit and comfortably above both platforms'
        // depth-127 proof while the runtime guard—not native exhaustion—owns
        // the depth-128 result.
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let at_limit =
                run("fn descend(n:int){ if n == 0 { 0 } else { descend(n - 1) } }\ndescend(127)");
            let beyond =
                run("fn descend(n:int){ if n == 0 { 0 } else { descend(n - 1) } }\ndescend(128)")
                    .expect_err("the 129th nested call must hit the typed guard");
            (at_limit, beyond.code, beyond.msg)
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(at_limit.unwrap(), Value::Int(0));
    assert_eq!(code, "recursion_limit");
    assert_eq!(
        message,
        format!(
            "maximum call depth of {} exceeded",
            crate::call::MAX_CALL_DEPTH
        )
    );
}

#[test]
fn mutual_recursion_guard_returns_error_on_explicit_stack() {
    let (code, message) = std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let error = run("fn left(){ right() }\nfn right(){ left() }\nleft()")
                .expect_err("mutual recursion must hit the typed guard");
            (error.code, error.msg)
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(code, "recursion_limit");
    assert_eq!(
        message,
        format!(
            "maximum call depth of {} exceeded",
            crate::call::MAX_CALL_DEPTH
        )
    );
}

#[test]
fn defect10_cd_inside_fn_body_is_rejected() {
    let err = run("fn f(){ cd / }\nf()").unwrap_err();
    assert_eq!(err.code, "custom");
    assert!(err.msg.contains("with cwd:"), "{}", err.msg);
}

#[test]
fn defect11_env_assignment_writes_session_env() {
    use shoal_ast::*;
    let s = Span::default();
    let target = Expr::Field {
        recv: Box::new(Expr::Var {
            name: "env".into(),
            span: s,
        }),
        name: "SHOAL_ASSIGNED".into(),
        optional: false,
        span: s,
    };
    let program = Program {
        stmts: vec![
            Stmt::Assign {
                target,
                op: AssignOp::Set,
                value: Expr::Str {
                    value: "bar".into(),
                    span: s,
                },
                span: s,
            },
            Stmt::Expr {
                expr: Expr::Field {
                    recv: Box::new(Expr::Var {
                        name: "env".into(),
                        span: s,
                    }),
                    name: "SHOAL_ASSIGNED".into(),
                    optional: false,
                    span: s,
                },
                span: s,
            },
        ],
    };
    let v = eval(&program, std::env::current_dir().unwrap()).unwrap();
    assert_eq!(v, Value::Str("bar".into()));
}

#[test]
fn defect11_env_assignment_rejected_in_fn_body() {
    use shoal_ast::*;
    let s = Span::default();
    let assign = Stmt::Assign {
        target: Expr::Field {
            recv: Box::new(Expr::Var {
                name: "env".into(),
                span: s,
            }),
            name: "X".into(),
            optional: false,
            span: s,
        },
        op: AssignOp::Set,
        value: Expr::Str {
            value: "1".into(),
            span: s,
        },
        span: s,
    };
    // fn f() { env.X = "1" }  then  f()
    let decl = FnDecl {
        name: "f".into(),
        params: vec![],
        rest: None,
        ret: None,
        body: Block {
            stmts: vec![assign],
            span: s,
        },
        doc: None,
        exported: false,
        span: s,
    };
    let program = Program {
        stmts: vec![
            Stmt::Fn { decl },
            Stmt::Expr {
                expr: Expr::FnCall {
                    name: "f".into(),
                    args: Args::empty(),
                    span: s,
                },
                span: s,
            },
        ],
    };
    let err = eval(&program, std::env::current_dir().unwrap()).unwrap_err();
    assert!(err.msg.contains("with env:"), "{}", err.msg);
}

#[test]
fn defect12_builtin_word_coercion() {
    // `sleep 0ms` binds the word to a duration; `sleep 0` to seconds. The
    // builtin now yields an outcome whose `.out` is null (P1a).
    assert_eq!(out_of(&run("sleep 0ms").unwrap()), Value::Null);
    assert_eq!(out_of(&run("sleep 0").unwrap()), Value::Null);
}

#[test]
fn defect12_fn_param_word_coercion() {
    // A bare CMD word binds to a typed fn param.
    let v = run("fn add1(n: int) { n + 1 }\nadd1 41").unwrap();
    assert_eq!(v, Value::Int(42));
}

#[test]
fn defect12_help_synthesis_returns_null() {
    let (out, captured) = run_capturing("fn deploy(env: str) { (env) }\ndeploy --help");
    assert_eq!(out.unwrap(), Value::Null);
    assert!(
        matches!(captured.last(), Some(Value::Str(s)) if s.contains("deploy") && s.contains("env")),
        "{captured:?}"
    );
}

#[test]
fn defect14_task_methods_and_jobs() {
    assert_eq!(
        run("let t = spawn { 2 + 3 }\nt.await()").unwrap(),
        Value::Int(5)
    );
    let is_done = run("let t = spawn { 7 }\nt.await()\nt.is_done()").unwrap();
    assert_eq!(is_done, Value::Bool(true));
    // `jobs` returns the registry table.
    let jobs = run("spawn { 1 }\njobs").unwrap();
    assert!(matches!(jobs, Value::Table(rows) if !rows.is_empty()));
}

#[test]
fn jobs_snapshot_separates_active_work_from_completed_history() {
    let mut ev = Evaluator::new(std::env::current_dir().unwrap());
    // Nothing spawned yet: a sane, zero-I/O empty snapshot.
    let empty = ev.jobs_snapshot();
    assert_eq!(empty.total, 0);
    assert_eq!(empty.running, 0);
    assert_eq!(empty.suspended, 0);
    assert_eq!(empty.completed, 0);

    // Awaiting every spawned task deterministically drives them to done,
    // so the post-await snapshot is a stable total/zero-running count.
    let prog = shoal_syntax::parse(
        "let a = spawn { 1 + 1 }\nlet b = spawn { 2 + 2 }\na.await()\nb.await()",
    )
    .unwrap();
    ev.eval_program(&prog).unwrap();
    let snap = ev.jobs_snapshot();
    assert_eq!(snap.total, 0, "completed tasks are not active prompt jobs");
    assert_eq!(snap.running, 0, "both were awaited to completion");
    assert_eq!(snap.completed, 2, "both remain in bounded job history");
}

#[test]
fn completed_job_history_is_bounded_without_invalidating_handles() {
    let ev = Evaluator::new(std::env::current_dir().unwrap());
    let mut handles = Vec::new();
    for index in 0..(crate::exec_state::MAX_COMPLETED_JOBS + 17) {
        let task = shoal_value::TaskVal::new(format!("completed-{index}"));
        task.finish(Ok(Value::Int(index as i64)));
        ev.exec.jobs.register(task.clone());
        handles.push(task);
    }

    let snapshot = ev.jobs_snapshot();
    assert_eq!(snapshot.total, 0);
    assert_eq!(snapshot.completed, crate::exec_state::MAX_COMPLETED_JOBS);
    let Value::Table(rows) = ev.jobs_table() else {
        panic!("jobs must remain a table")
    };
    assert_eq!(rows.len(), crate::exec_state::MAX_COMPLETED_JOBS);
    assert_eq!(
        handles[0].wait().unwrap(),
        Value::Int(0),
        "a handle remains valid after its registry row is pruned"
    );
}

#[test]
fn active_and_suspended_jobs_are_never_pruned() {
    let ev = Evaluator::new(std::env::current_dir().unwrap());
    let running = shoal_value::TaskVal::new("running");
    let stopped = shoal_value::TaskVal::new("stopped");
    stopped.mark_suspended();
    ev.exec.jobs.register(running);
    ev.exec.jobs.register(stopped);

    let snapshot = ev.jobs_snapshot();
    assert_eq!(snapshot.running, 1);
    assert_eq!(snapshot.suspended, 1);
    assert_eq!(snapshot.total, 2);
    assert_eq!(snapshot.completed, 0);
}

#[test]
fn echo_renders_non_scalar_values() {
    let v = run("let items = [1,2,3]\necho (items)").unwrap();
    assert_eq!(out_of(&v), Value::Str("[1, 2, 3]".into()));
}

#[test]
fn record_transcript_binds_it_and_out() {
    // `it`/`out` are REPL-only at parse time, so this transcript test
    // parses in REPL context.
    let repl = |src: &str| {
        shoal_syntax::parse_with_ctx(
            src,
            shoal_syntax::ParseCtx {
                repl: true,
                ..Default::default()
            },
        )
        .unwrap()
    };
    let mut ev = Evaluator::new(std::env::current_dir().unwrap());
    ev.record_transcript(&Value::Int(7)).unwrap();
    ev.record_transcript(&Value::Str("hi".into())).unwrap();
    let it = ev.eval_program(&repl("it")).unwrap();
    assert_eq!(it, Value::Str("hi".into()));
    let out = ev.eval_program(&repl("out")).unwrap();
    assert_eq!(
        out,
        Value::List(vec![Value::Int(7), Value::Str("hi".into())])
    );
}

#[test]
fn record_transcript_bounds_it_and_out_together() {
    let mut ev = Evaluator::new(std::env::current_dir().unwrap());
    for value in 0..=MAX_REPL_TRANSCRIPT_VALUES {
        ev.record_transcript(&Value::Int(value as i64)).unwrap();
    }
    assert_eq!(ev.it(), &Value::Int(MAX_REPL_TRANSCRIPT_VALUES as i64));
    let Some(Value::List(out)) = ev.env().get("out") else {
        panic!("out binding is not a list")
    };
    assert_eq!(out.len(), MAX_REPL_TRANSCRIPT_VALUES);
    assert_eq!(out.first(), Some(&Value::Int(1)));
    assert_eq!(
        out.last(),
        Some(&Value::Int(MAX_REPL_TRANSCRIPT_VALUES as i64))
    );
}

#[test]
fn record_transcript_failure_leaves_it_and_out_unchanged() {
    let mut ev = Evaluator::new(std::env::current_dir().unwrap());
    ev.record_transcript(&Value::Int(7)).unwrap();
    let retained = Value::List(vec![
        Value::Str("x".repeat(800));
        MAX_REPL_TRANSCRIPT_VALUES
    ]);
    ev.env()
        .declare("out", retained.clone(), true)
        .expect("the baseline transcript fits its per-binding wall");

    let error = ev
        .record_transcript(&Value::Str("y".repeat(1024 * 1024)))
        .expect_err("the expanded out list exceeds its per-binding wall");
    assert_eq!(error.code, "binding_value_limit");
    assert_eq!(ev.it(), &Value::Int(7));
    assert_eq!(ev.env().get("it"), Some(Value::Int(7)));
    assert_eq!(ev.env().get("out"), Some(retained));
}

#[test]
fn builtin_retry_and_parallel_and_save() {
    assert_eq!(run("retry(3, () => 42)").unwrap(), Value::Int(42));
    assert_eq!(
        run("parallel(() => 1, () => 2)").unwrap(),
        Value::List(vec![Value::Int(1), Value::Int(2)])
    );
    let dir = tempfile::tempdir().unwrap();
    run_in("save(\"out.txt\", \"payload\")", dir.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "payload"
    );
}

#[test]
fn builtin_retry_eventually_surfaces_error() {
    let err = run("retry(2, () => missing_command_xyz)").unwrap_err();
    assert!(
        err.code == "undefined_var" || err.code == "not_found",
        "{}",
        err.code
    );
}

#[test]
fn arithmetic_and_binding() {
    assert_eq!(run("let x = 2 + 3\nx * 4").unwrap(), Value::Int(20));
}

#[test]
fn strict_conditions_and_short_circuit() {
    assert_eq!(
        run("false && missing\ntrue || missing").unwrap(),
        Value::Bool(true)
    );
    assert_eq!(run("if true { 7 } else { 9 }").unwrap(), Value::Int(7));
    assert_eq!(run("if [1] { 2 }").unwrap_err().code, "type_error");
}

#[test]
fn functions_are_callable() {
    assert_eq!(
        run("fn twice(x: int) { x * 2 }\ntwice(21)").unwrap(),
        Value::Int(42)
    );
}

#[test]
fn captured_external_outcome_is_structured() {
    let value = run("let r = sh { printf hello }\nr.out").unwrap();
    assert_eq!(value, Value::Str("hello".into()));
}

#[test]
fn failed_statement_preserves_process_diagnostics() {
    let err = run("sh { printf boom >&2; exit 7 }").unwrap_err();
    assert_eq!(err.code, "cmd_failed");
    assert_eq!(err.status, Some(7));
    assert_eq!(err.stderr.as_deref(), Some("boom"));
}

#[test]
fn failed_statement_routes_captured_stdout_before_raising() {
    let (result, captured) = run_capturing("/bin/sh -c 'printf stdout-before-failure; exit 7'");
    let error = result.expect_err("nonzero statement must still raise");
    assert_eq!(error.code, "cmd_failed");
    assert_eq!(error.status, Some(7));
    assert_eq!(captured.len(), 1);
    let Value::Outcome(outcome) = &captured[0] else {
        panic!(
            "failed external must route its outcome, got {:?}",
            captured[0]
        );
    };
    assert!(!outcome.ok);
    assert_eq!(outcome.stdout.as_slice(), b"stdout-before-failure");
}
