use super::*;

// --- task suspend / resume ------------------------------------------------

#[test]
fn task_suspend_resume_methods_and_entrypoints() {
    // Value-method surface: `.suspend()`/`.resume()`/`.is_suspended()`.
    let v = run("let t = spawn { sleep 0ms\n1 }\nt.suspend()\nt.is_suspended()").unwrap();
    assert_eq!(v, Value::Bool(true));
    let v =
        run("let t = spawn { sleep 0ms\n1 }\nt.suspend()\nt.resume()\nt.is_suspended()").unwrap();
    assert_eq!(v, Value::Bool(false));

    // Kernel-callable entry points + jobs snapshot accounting.
    let mut ev = Evaluator::new(std::env::current_dir().unwrap());
    let prog = shoal_syntax::parse("spawn { sleep 5s }").unwrap();
    let task = ev.eval_program(&prog).unwrap();
    let Value::Task(t) = task else { panic!() };
    assert!(ev.suspend_task(t.id));
    assert!(t.is_suspended());
    assert_eq!(ev.jobs_snapshot().suspended, 1);
    assert!(ev.resume_task(t.id));
    assert!(!t.is_suspended());
    assert!(!ev.suspend_task(999_999));
    // fg lookup resolves a live task.
    assert!(ev.task_by_id(t.id).is_some());
    t.cancel();
}

/// A foreground external command stopped by Ctrl-Z (site/content/internals/language-conformance-contract.md) is recorded
/// as a `stopped` job that lists alongside spawned tasks, resolves to its
/// pid for `fg`/`bg`, and walks running↔stopped→done as the REPL drives it —
/// all without a real process (this test only exercises the jobs-table
/// bookkeeping, never a SIGTSTP/SIGCONT hook). The underlying OS mechanics
/// this bookkeeping represents — `WUNTRACED`/`WIFSTOPPED` mapping to a
/// stopped `ExecResult`, `SIGCONT` resuming a real stopped child to
/// completion, the `PARKED_JOBS` registry (`take_stopped_job` exactly once,
/// `shutdown_stopped_jobs` draining without a leak), and the `reaped` guard
/// against re-signalling an already-reaped/pid-recycled job — are covered
/// against the OS with real child processes in
/// `crates/shoal-exec/src/pty.rs`'s own `#[cfg(test)] mod tests`. What
/// remains untested anywhere (needs a real controlling terminal, so it's a
/// manual-verification gap, not an automatable one) is the live end-to-end
/// round trip: a user's actual Ctrl-Z keystroke being turned into `SIGTSTP`
/// by the pty line discipline, through the REPL prompt, to `fg`/`bg`.
#[test]
fn stopped_external_command_lists_and_transitions_in_the_jobs_table() {
    fn job_state(ev: &Evaluator, id: u64) -> Option<String> {
        let Value::Table(rows) = ev.jobs_table() else {
            return None;
        };
        rows.iter()
            .find(|r| matches!(r.get("id"), Some(Value::Int(n)) if *n as u64 == id))
            .and_then(|r| match r.get("state") {
                Some(Value::Str(s)) => Some(s.clone()),
                _ => None,
            })
    }

    let mut ev = Evaluator::new(std::env::current_dir().unwrap());
    let id = ev.register_stopped_external(4242, 4242, "sleep 30".into());

    // The pending-stop notice is queued for the REPL exactly once.
    assert_eq!(ev.take_pending_stop(), Some((id, "sleep 30".to_string())));
    assert_eq!(ev.take_pending_stop(), None);

    // It resolves to its pid (for `fg`/`bg`) and shows as `stopped`.
    assert_eq!(ev.external_job_pid(id), Some(4242));
    assert_eq!(ev.last_external_job(), Some(id));
    assert_eq!(job_state(&ev, id).as_deref(), Some("stopped"));
    assert_eq!(ev.jobs_snapshot().suspended, 1);

    // Resuming (`fg`/`bg`) flips it back to running without signalling.
    assert!(ev.mark_external_resumed(id));
    assert_eq!(job_state(&ev, id).as_deref(), Some("running"));
    assert_eq!(ev.jobs_snapshot().running, 1);

    // A re-stop (`fg`'d then Ctrl-Z'd again) re-arms the notice + state.
    ev.mark_external_stopped(id);
    assert_eq!(job_state(&ev, id).as_deref(), Some("stopped"));
    assert_eq!(ev.take_pending_stop(), Some((id, "sleep 30".to_string())));

    // Finishing retires it: `done`, and no longer resolvable for `fg`/`bg`.
    assert!(ev.finish_external_job(id));
    assert_eq!(job_state(&ev, id).as_deref(), Some("done"));
    assert_eq!(ev.external_job_pid(id), None);
    assert_eq!(ev.last_external_job(), None);
    assert!(!ev.mark_external_resumed(999_999), "unknown id is a no-op");
}

#[test]
fn now_and_today_are_live_datetime_anchors() {
    // `now`/`today` (site/content/internals/language-conformance-contract.md) resolve to a datetime, not an undefined var.
    let this_year = jiff::Zoned::now().year() as i64;
    assert_eq!(run("now.year").unwrap(), Value::Int(this_year));
    assert_eq!(run("today.year").unwrap(), Value::Int(this_year));
    assert_eq!(run("now().year").unwrap(), Value::Int(this_year));
    // `today` is midnight: hour/minute/second all zero.
    assert_eq!(run("today.hour").unwrap(), Value::Int(0));
    assert_eq!(run("today.minute").unwrap(), Value::Int(0));
    // A user binding still shadows the anchor name.
    assert_eq!(run("let now = 5\nnow").unwrap(), Value::Int(5));
}

#[test]
fn duration_ago_and_from_now_compose_to_datetime() {
    // `.ago` is in the past, `.from_now` in the future (site/content/internals/language-conformance-contract.md).
    assert!(matches!(run("1h.ago").unwrap(), Value::DateTime(_)));
    assert!(matches!(run("30d.from_now").unwrap(), Value::DateTime(_)));
    // from_now is strictly after ago for the same duration.
    assert_eq!(run("1h.from_now > 1h.ago").unwrap(), Value::Bool(true));
    // Round-trips through datetime arithmetic: now + 1h ~ 1h.from_now.
    assert_eq!(run("1h.from_now > now").unwrap(), Value::Bool(true));
    assert_eq!(run("1h.ago < now").unwrap(), Value::Bool(true));
    // An unknown duration field is still a plain field_missing.
    assert_eq!(run("1h.nope").unwrap_err().code, "field_missing");
}

#[test]
fn assert_builtin_raises_assert_failed() {
    // False condition → assert_failed (site/content/internals/intercrate-protocol-contracts.md).
    let e = run("assert(1 == 2)").unwrap_err();
    assert_eq!(e.code, "assert_failed");
    // Custom message is carried through.
    let e = run(r#"assert(false, "boom")"#).unwrap_err();
    assert_eq!(e.code, "assert_failed");
    assert_eq!(e.msg, "boom");
    // True condition → null, no raise.
    assert_eq!(run("assert(1 == 1)").unwrap(), Value::Null);
    // Command-head spelling works too.
    assert_eq!(run("assert (2 > 1)").unwrap(), Value::Null);
    // Non-bool condition is a type_error, not a silent pass.
    assert_eq!(run("assert(3)").unwrap_err().code, "type_error");
}

#[test]
fn list_path_param_receives_all_glob_matches() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "").unwrap();
    std::fs::write(dir.path().join("b.txt"), "").unwrap();
    // A non-variadic `list<path>` param gets every sorted match (site/content/internals/language-conformance-contract.md).
    let v = run_in(
        "fn showpaths(paths: list<path>) { paths.len() }\nshowpaths *.txt",
        dir.path(),
    )
    .unwrap();
    assert_eq!(v, Value::Int(2));
}

#[test]
fn glob_excludes_dotfiles_by_default() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".hidden.txt"), "").unwrap();
    std::fs::write(dir.path().join("a.txt"), "").unwrap();
    std::fs::write(dir.path().join("b.txt"), "").unwrap();
    // Plain `*.txt` skips `.hidden.txt` (site/content/internals/language-conformance-contract.md): 2, not 3.
    let v = run_in(
        "fn f(...rest: list<path>) { rest.len() }\nf *.txt",
        dir.path(),
    )
    .unwrap();
    assert_eq!(v, Value::Int(2));
    // A dot-leading pattern opts back in.
    let v = run_in(
        "fn f(...rest: list<path>) { rest.len() }\nf .*.txt",
        dir.path(),
    )
    .unwrap();
    assert_eq!(v, Value::Int(1));
}

#[test]
fn alias_appends_later_flags_to_adapter_call() {
    // `alias gs = git status; (gs --short).cmd` must carry `--short`
    // through to the resolved argv (site/content/internals/language-conformance-contract.md), not drop it.
    let v = run("alias gs = git status\n(gs --short).cmd").unwrap();
    assert_eq!(v, Value::Str("git status --short".into()));
}

#[test]
fn run_unresolvable_extension_raises_runner_not_found() {
    // No `[runners]` entry and no shebang for `.zzz` → runner_not_found
    // (site/content/internals/values-streams-execution.md step 3), not a bare filesystem not_found.
    let e = run(r#"run("./definitely-not-a-real-script-xyz.zzz")"#).unwrap_err();
    assert_eq!(e.code, "runner_not_found");
}

#[test]
fn background_ampersand_yields_a_task() {
    // `cmd &` desugars to `spawn { cmd }` (site/content/internals/language-conformance-contract.md): yields a task.
    let v = run("let t = (echo hi &)\nt.await()\nt.is_done()").unwrap();
    assert_eq!(v, Value::Bool(true));
    // Value-position `&` produces a task handle directly.
    assert!(matches!(run("(echo hi &)").unwrap(), Value::Task(_)));
    // The awaited task's outcome is the command's stdout.
    assert_eq!(
        run("let t = (echo hi &)\nt.await().out").unwrap(),
        Value::Str("hi".into())
    );
}

#[test]
fn path_filesystem_methods_read_lines_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), b"alpha\nbeta\r\ngamma\n").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    // `.read()` resolves relative to cwd and returns the whole file as str.
    assert_eq!(
        run_in(r#"path("data.txt").read()"#, dir.path()).unwrap(),
        Value::Str("alpha\nbeta\r\ngamma\n".into())
    );
    // `.read_bytes()` yields raw bytes.
    assert!(matches!(
        run_in(r#"path("data.txt").read_bytes()"#, dir.path()).unwrap(),
        Value::Bytes(b) if b.len() == 18
    ));
    // `.lines()` splits and strips CR, and composes with list methods.
    assert_eq!(
        run_in(r#"path("data.txt").lines()"#, dir.path()).unwrap(),
        Value::List(vec![
            Value::Str("alpha".into()),
            Value::Str("beta".into()),
            Value::Str("gamma".into()),
        ])
    );
    assert_eq!(
        run_in(r#"path("data.txt").lines().first(2)"#, dir.path()).unwrap(),
        Value::List(vec![Value::Str("alpha".into()), Value::Str("beta".into())])
    );
    // `.exists()`/`.is_file()`/`.is_dir()`.
    assert_eq!(
        run_in(r#"path("data.txt").exists()"#, dir.path()).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        run_in(r#"path("nope.txt").exists()"#, dir.path()).unwrap(),
        Value::Bool(false)
    );
    assert_eq!(
        run_in(r#"path("data.txt").is_file()"#, dir.path()).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        run_in(r#"path("sub").is_dir()"#, dir.path()).unwrap(),
        Value::Bool(true)
    );
    // `.size()` is a size.
    assert_eq!(
        run_in(r#"path("data.txt").size()"#, dir.path()).unwrap(),
        Value::Size(18)
    );
    // `.modified()` is a datetime.
    assert!(matches!(
        run_in(r#"path("data.txt").modified()"#, dir.path()).unwrap(),
        Value::DateTime(_)
    ));
    // A missing file surfaces `not_found`, not a panic.
    assert_eq!(
        run_in(r#"path("nope.txt").read()"#, dir.path())
            .unwrap_err()
            .code,
        "not_found"
    );
}

#[test]
fn oversized_sparse_path_read_is_typed_and_the_evaluator_recovers() {
    let directory = tempfile::tempdir().unwrap();
    let file = std::fs::File::create(directory.path().join("sparse.bin")).unwrap();
    file.set_len((crate::path_access::MAX_PATH_READ_BYTES + 1) as u64)
        .unwrap();
    drop(file);

    let mut evaluator = Evaluator::new(directory.path().to_path_buf());
    let read = shoal_syntax::parse(r#"path("sparse.bin").read()"#).unwrap();
    let error = evaluator.eval_program(&read).unwrap_err();
    assert_eq!(error.code, "path_read_limit");
    assert!(error.msg.contains("sparse.bin"));
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("stream"))
    );

    assert_eq!(
        evaluator
            .eval_program(&shoal_syntax::parse("40 + 2").unwrap())
            .unwrap(),
        Value::Int(42),
        "rejecting an oversized eager read must leave the evaluator usable"
    );
}

#[test]
fn path_pure_component_methods() {
    // Pure component accessors need no filesystem.
    assert_eq!(
        run(r#"path("/a/b/file.txt").name()"#).unwrap(),
        Value::Str("file.txt".into())
    );
    assert_eq!(
        run(r#"path("/a/b/file.txt").stem()"#).unwrap(),
        Value::Str("file".into())
    );
    assert_eq!(
        run(r#"path("/a/b/file.txt").ext()"#).unwrap(),
        Value::Str("txt".into())
    );
    assert_eq!(
        run(r#"path("/a/b/file.txt").parent()"#).unwrap(),
        Value::Path("/a/b".into())
    );
    assert_eq!(
        run(r#"path("/a/b").join("c")"#).unwrap(),
        Value::Path("/a/b/c".into())
    );
    // `.ext()` of an extensionless name is null.
    assert_eq!(run(r#"path("/a/README").ext()"#).unwrap(), Value::Null);
}

#[test]
fn glob_value_behaves_as_collection() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), b"").unwrap();
    std::fs::write(dir.path().join("b.rs"), b"").unwrap();
    std::fs::write(dir.path().join("c.txt"), b"").unwrap();

    // `.len()` expands and counts (sorted, cwd-relative).
    assert_eq!(
        run_in(r#"glob("*.rs").len()"#, dir.path()).unwrap(),
        Value::Int(2)
    );
    // `.expand()` yields the sorted match list.
    assert_eq!(
        run_in(r#"glob("*.rs").expand().len()"#, dir.path()).unwrap(),
        Value::Int(2)
    );
    // `.pattern` (field and method) returns the source pattern.
    assert_eq!(
        run_in(r#"glob("*.rs").pattern"#, dir.path()).unwrap(),
        Value::Str("*.rs".into())
    );
    // `.map(...)` re-dispatches on the expanded list.
    assert_eq!(
        run_in(r#"glob("*.rs").map(.name())"#, dir.path()).unwrap(),
        Value::List(vec![Value::Str("a.rs".into()), Value::Str("b.rs".into())])
    );
    // `for x in <glob>` iterates the expanded matches. (The glob value is
    // parenthesized only to sidestep a parser limitation shared by every
    // `)`-terminated call in a for-in head — the iteration itself is the
    // glob-value path exercised here.)
    let (_out, captured) =
        run_capturing_in(r#"for f in (glob("*.rs")) { echo (f.name()) }"#, dir.path());
    let texts: Vec<Value> = captured.iter().map(out_of).collect();
    assert_eq!(
        texts,
        vec![Value::Str("a.rs".into()), Value::Str("b.rs".into())]
    );
}
