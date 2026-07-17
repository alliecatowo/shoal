
use super::*;

#[test]
fn repl_orchestration_stays_decomposed() {
    let root = include_str!("../repl.rs");
    assert!(
        root.lines().count() <= 360,
        "REPL root regrew past the orchestration-only ceiling"
    );
    for (start, end, limit) in [
        ("pub(crate) fn repl(", "struct InterruptState", 130),
        ("fn run_repl_loop(", "fn handle_submitted_line(", 70),
        ("fn handle_submitted_line(", "fn parse_ctx_for(", 110),
    ] {
        let body = root
            .split_once(start)
            .and_then(|(_, tail)| tail.split_once(end).map(|(body, _)| body))
            .unwrap_or_else(|| panic!("missing structural markers {start:?} -> {end:?}"));
        assert!(
            body.lines().count() <= limit,
            "{start} regrew to {} lines (limit {limit})",
            body.lines().count()
        );
    }
    for (name, source, limit) in [
        ("jobs", include_str!("jobs.rs"), 700),
        ("protocol", include_str!("protocol.rs"), 240),
        ("transcript", include_str!("transcript.rs"), 240),
        ("ui", include_str!("ui.rs"), 200),
    ] {
        assert!(
            source.lines().count() <= limit,
            "REPL {name} module regrew to {} lines (limit {limit})",
            source.lines().count()
        );
    }
}

#[test]
fn journal_config_resolves_paths_and_only_gates_language_history() {
    let cwd = Path::new("/work/project");
    assert_eq!(
        effective_journal_state_dir(Some(Path::new(".state/shoal")), cwd),
        PathBuf::from("/work/project/.state/shoal")
    );
    assert_eq!(
        effective_journal_state_dir(Some(Path::new("/var/lib/shoal")), cwd),
        PathBuf::from("/var/lib/shoal")
    );
    assert!(language_journal_requested(true, false));
    assert!(!language_journal_requested(false, false));
    assert!(
        !language_journal_requested(true, true),
        "protocol sessions install language history in the kernel"
    );
}

struct FakeProtocol {
    seen: Vec<String>,
    outcome: Result<ProtocolOutcome, String>,
    snapshot: Result<serde_json::Value, String>,
}

impl ReplProtocol for FakeProtocol {
    fn execute(
        &mut self,
        src: &str,
        _interrupt: &AtomicBool,
        _width: usize,
    ) -> Result<ProtocolOutcome, String> {
        self.seen.push(src.to_string());
        self.outcome.clone()
    }

    fn snapshot(&mut self) -> Result<serde_json::Value, String> {
        self.snapshot.clone()
    }
}

fn unused_snapshot() -> Result<serde_json::Value, String> {
    Err("snapshot not used by line-level test".into())
}

fn protocol_outcome(render: Option<&str>, state: &str) -> ProtocolOutcome {
    ProtocolOutcome {
        value_ref: Some("out:1".into()),
        render: render.map(str::to_owned),
        state: state.into(),
        exit_code: None,
        streamed: false,
    }
}

#[test]
fn explicit_standalone_and_disabled_kernel_never_route_to_protocol() {
    assert!(!protocol_requested(true, true));
    assert!(!protocol_requested(true, false));
    assert!(!protocol_requested(false, false));
    assert!(protocol_requested(false, true));
}

#[test]
fn protocol_line_preserves_source_and_applies_task_fg_sugar() {
    let mut protocol = FakeProtocol {
        seen: Vec::new(),
        outcome: Ok(protocol_outcome(Some("42"), "completed")),
        snapshot: unused_snapshot(),
    };
    let interrupt = AtomicBool::new(false);
    assert_eq!(
        execute_protocol_line(&mut protocol, "40 + 2", &interrupt, 120)
            .unwrap()
            .render
            .as_deref(),
        Some("42")
    );
    execute_protocol_line(&mut protocol, "fg worker", &interrupt, 120).unwrap();
    assert_eq!(protocol.seen, ["40 + 2", "worker.resume()\nworker.await()"]);
}

#[test]
fn protocol_line_rejects_local_process_group_control_before_rpc() {
    let mut protocol = FakeProtocol {
        seen: Vec::new(),
        outcome: Ok(protocol_outcome(None, "completed")),
        snapshot: unused_snapshot(),
    };
    let error =
        execute_protocol_line(&mut protocol, "fg %2", &AtomicBool::new(false), 80).unwrap_err();
    assert!(error.contains("--standalone"));
    assert!(protocol.seen.is_empty());
}

#[test]
fn cancelled_protocol_outcomes_never_render_stale_values() {
    let completed = protocol_outcome(Some("done"), "completed");
    assert_eq!(protocol_render_text(&completed), Some("done"));
    let cancelled = protocol_outcome(Some("stale"), "cancelled");
    assert_eq!(protocol_render_text(&cancelled), None);
    let empty = protocol_outcome(Some(""), "completed");
    assert_eq!(protocol_render_text(&empty), None);
    let mut streamed = protocol_outcome(Some("already live"), "completed");
    streamed.streamed = true;
    assert_eq!(protocol_render_text(&streamed), None);
}

#[test]
fn protocol_exit_status_is_carried_to_the_ui_boundary() {
    let mut outcome = protocol_outcome(None, "completed");
    outcome.exit_code = Some(17);
    let mut protocol = FakeProtocol {
        seen: Vec::new(),
        outcome: Ok(outcome),
        snapshot: unused_snapshot(),
    };
    assert_eq!(
        execute_protocol_line(&mut protocol, "exit 17", &AtomicBool::new(false), 80)
            .unwrap()
            .exit_code,
        Some(17)
    );
}

#[test]
fn protocol_snapshot_refreshes_completion_env_and_cwd() {
    let mut protocol = FakeProtocol {
        seen: Vec::new(),
        outcome: Ok(protocol_outcome(None, "completed")),
        snapshot: Ok(serde_json::json!({
            "cwd": {"display": "/remote/project"},
            "completion": {"path_dirs": [{"display": "/remote/project/bin"}]},
            "bindings": [
                {"name": "deploy", "callable": true, "type": "command"},
                {"name": "answer", "callable": false, "type": "int"}
            ],
            "jobs": {"running": 1, "suspended": 0, "total": 1},
            "reef": {"bindings": []},
            "last_value": {"$": "null"}
        })),
    };
    let env = Env::root();
    let cwd = Arc::new(Mutex::new(PathBuf::new()));
    let path_dirs = Arc::new(Mutex::new(None));
    let snapshot = refresh_protocol_state(
        &mut protocol,
        &mut RemoteEnvMirror::default(),
        &env,
        &cwd,
        &path_dirs,
    )
    .unwrap();

    assert_eq!(snapshot.jobs.running, 1);
    assert_eq!(*cwd.lock().unwrap(), PathBuf::from("/remote/project"));
    assert_eq!(
        *path_dirs.lock().unwrap(),
        Some(vec![PathBuf::from("/remote/project/bin")])
    );
    assert!(env.get("deploy").is_some_and(|value| value.is_callable()));
    assert!(matches!(env.get("answer"), Some(Value::Int(0))));
}

#[test]
fn journal_id_mirror_evicts_in_lockstep_with_evaluator_out() {
    let mut entries = (0..shoal_eval::MAX_REPL_TRANSCRIPT_VALUES)
        .map(|id| Some(id as i64))
        .collect::<VecDeque<_>>();
    push_out_entry(&mut entries, Some(9_999));
    assert_eq!(entries.len(), shoal_eval::MAX_REPL_TRANSCRIPT_VALUES);
    assert_eq!(entries.front(), Some(&Some(1)));
    assert_eq!(entries.back(), Some(&Some(9_999)));
}

#[test]
fn multiline_detection_ignores_balanced_delimiters_in_strings_and_comments() {
    assert!(input_is_incomplete("if true {\n  1"));
    assert!(input_is_incomplete("1 +"));
    assert!(input_is_incomplete("\"unterminated"));
    assert!(input_is_incomplete("\"\"\"unterminated"));
    assert!(!input_is_incomplete("\"\"\"multiline\ntext\"\"\""));
    assert!(!input_is_incomplete("echo \"{\""));
    assert!(!input_is_incomplete("# {\n1"));
    assert!(!input_is_incomplete("[1, 2]"));
}

/// Job-control line recognition (site/content/internals/language-conformance-contract.md): bare `fg`/`bg` and `%N`/`N`
/// forms are job control; an identifier arg, a longer command sharing the
/// prefix, or an assignment must fall through untouched.
#[test]
fn parse_job_control_matches_only_fg_bg_verbs() {
    let fg = |s: &str| parse_job_control(s).map(|jc| (matches!(jc.kind, JobKind::Fg), jc.id));
    assert_eq!(fg("fg"), Some((true, None)));
    assert_eq!(fg("  fg  "), Some((true, None)));
    assert_eq!(fg("fg 2"), Some((true, Some(2))));
    assert_eq!(fg("fg %3"), Some((true, Some(3))));
    assert_eq!(fg("bg"), Some((false, None)));
    assert_eq!(fg("bg 5"), Some((false, Some(5))));
    // Not job control: an identifier arg (that is `rewrite_fg`'s domain),
    // a command that merely starts with the letters, or an assignment.
    assert!(parse_job_control("fg mytask").is_none());
    assert!(parse_job_control("fgrep pattern").is_none());
    assert!(parse_job_control("bgtool").is_none());
    assert!(parse_job_control("fg=1").is_none());
    assert!(parse_job_control("echo hi").is_none());
}

#[test]
fn background_job_events_reconcile_evaluator_rows() {
    let mut evaluator = Evaluator::new(PathBuf::from("/"));
    let completed = evaluator.register_stopped_external(41_001, 41_001, "done-job".into());
    assert!(evaluator.mark_external_resumed(completed));
    let (tx, rx) = mpsc::sync_channel(MAX_PENDING_BACKGROUND_EVENTS);
    tx.send(BackgroundJobEvent::Completed {
        id: completed,
        command: "done-job".into(),
        status: Some(0),
        signal: None,
        omitted_output_bytes: 0,
        notified: false,
    })
    .unwrap();
    drain_background_job_events(&mut evaluator, &rx);
    assert_eq!(evaluator.external_job_pid(completed), None);
    assert!(evaluator.task_by_id(completed).unwrap().is_done());

    let failed = evaluator.register_stopped_external(41_003, 41_003, "failed-job".into());
    assert!(evaluator.mark_external_resumed(failed));
    tx.send(BackgroundJobEvent::Completed {
        id: failed,
        command: "failed-job".into(),
        status: Some(7),
        signal: None,
        omitted_output_bytes: 0,
        notified: false,
    })
    .unwrap();
    drain_background_job_events(&mut evaluator, &rx);
    let error = evaluator
        .task_by_id(failed)
        .unwrap()
        .wait()
        .expect_err("nonzero background exit must remain a failed task");
    assert_eq!(error.code, "cmd_failed");
    assert_eq!(error.status, Some(7));

    let stopped = evaluator.register_stopped_external(41_002, 41_002, "stopped-job".into());
    assert!(evaluator.mark_external_resumed(stopped));
    tx.send(BackgroundJobEvent::Stopped {
        id: stopped,
        command: "stopped-job".into(),
        omitted_output_bytes: 0,
        notified: false,
    })
    .unwrap();
    drain_background_job_events(&mut evaluator, &rx);
    assert_eq!(evaluator.external_job_pid(stopped), Some(41_002));
    assert!(evaluator.task_by_id(stopped).unwrap().is_suspended());
    assert_eq!(evaluator.last_stopped_external(), Some(stopped));
}

#[test]
fn background_notices_use_a_bounded_nonblocking_reedline_queue() {
    let printer = ExternalPrinter::new(1);
    let mut first = BackgroundJobEvent::Completed {
        id: 7,
        command: "fast-job".into(),
        status: Some(0),
        signal: None,
        omitted_output_bytes: 0,
        notified: false,
    };
    enqueue_background_notice(&printer, &mut first);
    assert!(first.notified());
    assert!(printer.get_line().unwrap().contains("[7]+  Done"));

    // Fill the capacity, then prove a PTY callback falls back instead of
    // blocking when Reedline has not consumed the earlier notice yet.
    printer.sender().try_send("occupied".into()).unwrap();
    let mut second = BackgroundJobEvent::Failed {
        id: 8,
        error: "boom".into(),
        omitted_output_bytes: 0,
        notified: false,
    };
    enqueue_background_notice(&printer, &mut second);
    assert!(!second.notified());
}

#[test]
fn background_transition_queue_applies_bounded_lossless_backpressure() {
    let (tx, _rx) = mpsc::sync_channel(MAX_PENDING_BACKGROUND_EVENTS);
    for id in 0..MAX_PENDING_BACKGROUND_EVENTS as u64 {
        tx.try_send(BackgroundJobEvent::TaskCompleted {
            id,
            description: "done".into(),
            error: None,
            notified: true,
        })
        .unwrap();
    }
    assert!(matches!(
        tx.try_send(BackgroundJobEvent::TaskCompleted {
            id: u64::MAX,
            description: "bounded".into(),
            error: None,
            notified: true,
        }),
        Err(mpsc::TrySendError::Full(_))
    ));
}

#[test]
fn blocked_background_transition_producer_wakes_when_repl_receiver_drops() {
    let (tx, rx) = mpsc::sync_channel(1);
    tx.send(BackgroundJobEvent::TaskCompleted {
        id: 1,
        description: "fills queue".into(),
        error: None,
        notified: true,
    })
    .unwrap();
    let exited = Arc::new(AtomicBool::new(false));
    let worker_exited = exited.clone();
    let worker = std::thread::spawn(move || {
        assert!(
            tx.send(BackgroundJobEvent::TaskCompleted {
                id: 2,
                description: "blocked until shutdown".into(),
                error: None,
                notified: true,
            })
            .is_err()
        );
        worker_exited.store(true, Ordering::Release);
    });
    std::thread::sleep(Duration::from_millis(20));
    assert!(!exited.load(Ordering::Acquire));

    drop(rx);
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !exited.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(exited.load(Ordering::Acquire));
    worker.join().unwrap();
}

#[test]
fn background_output_is_line_bounded_and_terminal_control_safe() {
    let printer = ExternalPrinter::new(4);
    let mut output = BackgroundOutputState::new(12, printer.clone());
    output.push(b"safe\x1b[2Jtext\r\n");
    assert_eq!(output.finish(), 0);
    let line = printer.get_line().unwrap();
    assert!(line.starts_with("[12] safe"));
    assert!(line.contains("\\u{1b}[2Jtext"));
    assert!(!line.contains('\x1b'), "raw ESC must never reach Reedline");
    assert!(!line.contains('\r'), "CRLF is normalized by the event path");
}

#[test]
fn background_output_queue_saturation_is_counted_for_the_terminal_notice() {
    let printer = ExternalPrinter::new(1);
    printer.sender().try_send("occupied".into()).unwrap();
    let mut output = BackgroundOutputState::new(13, printer);
    output.push(b"dropped line\n");
    assert_eq!(output.finish(), b"dropped line\n".len());
}

#[test]
fn returned_taskvals_emit_one_async_completion_notice() {
    let mut evaluator = Evaluator::new(PathBuf::from("/"));
    let value = evaluator
        .eval_program(&shoal_syntax::parse("spawn { sleep 10ms\n42 }").unwrap())
        .unwrap();
    let Value::Task(task) = &value else {
        panic!("spawn must return a task")
    };
    let (tx, rx) = mpsc::sync_channel(MAX_PENDING_BACKGROUND_EVENTS);
    let printer = ExternalPrinter::new(4);
    let mut watched = BTreeSet::new();
    let suppressed = Arc::new(Mutex::new(BTreeSet::new()));
    watch_new_tasks(
        &evaluator,
        Some(&value),
        &mut watched,
        &suppressed,
        &tx,
        &printer,
    );
    let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        event,
        BackgroundJobEvent::TaskCompleted { id, error: None, notified: true, .. }
            if id == task.id
    ));
    assert!(printer.get_line().unwrap().contains("Done"));

    watch_new_tasks(
        &evaluator,
        Some(&value),
        &mut watched,
        &suppressed,
        &tx,
        &printer,
    );
    assert!(rx.try_recv().is_err(), "a task is watched exactly once");
}

#[test]
fn task_awaited_in_the_submitted_line_does_not_emit_a_background_notice() {
    let mut evaluator = Evaluator::new(PathBuf::from("/"));
    let value = evaluator
        .eval_program(&shoal_syntax::parse("let t = spawn { 42 }\nt.await()").unwrap())
        .unwrap();
    let (tx, rx) = mpsc::sync_channel(MAX_PENDING_BACKGROUND_EVENTS);
    let printer = ExternalPrinter::new(4);
    watch_new_tasks(
        &evaluator,
        Some(&value),
        &mut BTreeSet::new(),
        &Arc::new(Mutex::new(BTreeSet::new())),
        &tx,
        &printer,
    );
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    assert!(printer.get_line().is_none());
}

#[test]
fn host_task_id_mirrors_reclaim_monotonic_churn_without_retargeting() {
    let mut watched = (0..10_000).collect::<BTreeSet<u64>>();
    let current = [9_997, 9_999, 10_001].into_iter().collect();
    retain_current_task_ids(&mut watched, &current);
    assert_eq!(watched, [9_997, 9_999].into_iter().collect());

    let suppressed = Arc::new(Mutex::new((0..10_000).collect::<BTreeSet<u64>>()));
    for id in 0..10_000 {
        assert!(consume_task_suppression(&suppressed, id));
        assert!(
            !consume_task_suppression(&suppressed, id),
            "suppression IDs are one-shot and never retargeted"
        );
    }
    assert!(suppressed.lock().unwrap().is_empty());
}

#[test]
fn failed_task_watcher_launch_retires_mirror_and_warns_for_retry() {
    let printer = ExternalPrinter::new(1);
    let mut watched = [77].into_iter().collect();
    handle_task_watcher_launch(
        Err(std::io::Error::other("thread quota reached")),
        77,
        &mut watched,
        &printer,
    );
    assert!(!watched.contains(&77));
    let warning = printer.get_line().unwrap();
    assert!(warning.contains("cannot watch task [77]"));
    assert!(warning.contains("thread quota reached"));
}

#[test]
fn production_repl_has_no_infallible_thread_launches() {
    let production = include_str!("../repl.rs")
        .split("#[cfg(test)]")
        .next()
        .expect("production source prefix");
    assert!(!production.contains("std::thread::spawn("));
}

#[test]
fn parse_ctx_splits_values_from_callables() {
    let env = Env::root();
    env.declare("mydata", Value::Int(3), false);
    env.declare(
        "deploy",
        Value::CmdRef(Arc::new(shoal_ast::CmdCall {
            head: "echo".into(),
            forced: false,
            env_prefix: Vec::new(),
            args: Vec::new(),
            redirects: Vec::new(),
            background: false,
            trailing: None,
            span: shoal_ast::Span::new(0, 0),
        })),
        false,
    );
    let ctx = parse_ctx_for(&env);
    assert!(ctx.repl);
    assert!(ctx.value_bound.iter().any(|n| n == "mydata"));
    assert!(ctx.cmd_bound.iter().any(|n| n == "deploy"));
    assert!(!ctx.cmd_bound.iter().any(|n| n == "mydata"));
}

#[test]
fn glob_match_supports_star_and_question_wildcards() {
    assert!(glob_match("ls*", "ls -la"));
    assert!(glob_match(
        "* --password=*",
        "curl --password=secret --url=x"
    ));
    assert!(glob_match("g?t status", "git status"));
    assert!(!glob_match("g?t status", "goat status"));
    assert!(glob_match("*", ""));
    assert!(glob_match("exact", "exact"));
    assert!(!glob_match("exact", "exactly"));
}

#[test]
fn open_history_reports_an_uncreatable_parent() {
    let dir = tempfile::tempdir().unwrap();
    let blocking_file = dir.path().join("not-a-directory");
    fs::write(&blocking_file, b"x").unwrap();
    let error = open_history(100, &blocking_file.join("history"))
        .expect_err("a file cannot be used as a history parent directory");
    assert!(error.contains("cannot create history directory"), "{error}");
}

/// `history.dedup` (site/content/internals/configuration-reference.md): a line identical to the
/// immediately preceding one is skipped; a different line, or the same
/// line after a different one in between, is recorded.
#[test]
fn filtered_history_dedup_skips_only_immediate_repeats() {
    let dir = tempfile::tempdir().unwrap();
    let inner = FileBackedHistory::with_file(100, dir.path().join("hist")).unwrap();
    let mut history = FilteredHistory::new(Box::new(inner), true, Vec::new());
    history.save(HistoryItem::from_command_line("ls")).unwrap();
    history.save(HistoryItem::from_command_line("ls")).unwrap();
    history.save(HistoryItem::from_command_line("pwd")).unwrap();
    history.save(HistoryItem::from_command_line("ls")).unwrap();
    let all = history
        .search(SearchQuery::everything(SearchDirection::Forward, None))
        .unwrap();
    let lines: Vec<&str> = all.iter().map(|i| i.command_line.as_str()).collect();
    assert_eq!(
        lines,
        vec!["ls", "pwd", "ls"],
        "the immediate repeat must be dropped, but a later repeat after a \
             different line must not be"
    );
}

#[test]
fn filtered_history_clear_resets_dedup_state() {
    let dir = tempfile::tempdir().unwrap();
    let inner = FileBackedHistory::with_file(100, dir.path().join("hist")).unwrap();
    let mut history = FilteredHistory::new(Box::new(inner), true, Vec::new());
    history
        .save(HistoryItem::from_command_line("kept-after-clear"))
        .unwrap();
    history.clear().unwrap();
    history
        .save(HistoryItem::from_command_line("kept-after-clear"))
        .unwrap();

    let all = history
        .search(SearchQuery::everything(SearchDirection::Forward, None))
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].command_line, "kept-after-clear");
}

/// `history.ignore` (site/content/internals/configuration-reference.md, `HISTIGNORE`-equivalent): a line
/// matching any pattern is never recorded.
#[test]
fn filtered_history_ignore_patterns_are_never_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let inner = FileBackedHistory::with_file(100, dir.path().join("hist")).unwrap();
    let mut history = FilteredHistory::new(
        Box::new(inner),
        false,
        vec!["ls*".to_string(), "secret *".to_string()],
    );
    history
        .save(HistoryItem::from_command_line("ls -la"))
        .unwrap();
    history
        .save(HistoryItem::from_command_line("secret reveal"))
        .unwrap();
    history
        .save(HistoryItem::from_command_line("echo kept"))
        .unwrap();
    let all = history
        .search(SearchQuery::everything(SearchDirection::Forward, None))
        .unwrap();
    assert_eq!(all.len(), 1, "only the non-matching line should persist");
    assert_eq!(all[0].command_line, "echo kept");
}

#[test]
fn filtered_history_dedup_seeds_from_the_last_persisted_entry() {
    // A fresh `FilteredHistory` built over a backend that already has
    // entries (a new process attaching to an existing history file) must
    // still dedup against the *last* one, not just entries recorded in
    // this in-memory instance's own lifetime.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hist");
    {
        let mut inner = FileBackedHistory::with_file(100, path.clone()).unwrap();
        inner.save(HistoryItem::from_command_line("ls")).unwrap();
        inner.sync().unwrap();
    }
    let inner = FileBackedHistory::with_file(100, path).unwrap();
    let mut history = FilteredHistory::new(Box::new(inner), true, Vec::new());
    history.save(HistoryItem::from_command_line("ls")).unwrap();
    let all = history
        .search(SearchQuery::everything(SearchDirection::Forward, None))
        .unwrap();
    assert_eq!(
        all.len(),
        1,
        "the repeat of the last-persisted line must be deduped"
    );
}

/// `editor.mode` (site/content/internals/configuration-reference.md): `"vi"` selects reedline's `Vi` edit
/// mode, anything else (including the default `"emacs"`) selects `Emacs`.
#[test]
fn build_edit_mode_selects_vi_or_emacs_from_config() {
    let mut config = shoal_config::Config::default();
    config.editor.mode = "vi".to_string();
    let vi_mode = build_edit_mode(&config, &[]);
    assert!(matches!(
        vi_mode.edit_mode(),
        reedline::PromptEditMode::Vi(_)
    ));

    config.editor.mode = "emacs".to_string();
    let emacs_mode = build_edit_mode(&config, &[]);
    assert_eq!(emacs_mode.edit_mode(), reedline::PromptEditMode::Emacs);
}

/// `editor.keybindings` (site/content/internals/configuration-reference.md): a custom chord actually
/// fires its configured action through the real `EditMode::parse_event`
/// path, in both emacs and vi-insert mode.
#[test]
fn build_edit_mode_applies_custom_bindings() {
    use crossterm::event::{Event, KeyEvent};

    let custom = vec![crate::keybindings::ParsedBinding {
        modifiers: KeyModifiers::CONTROL,
        code: KeyCode::Char('g'),
        event: ReedlineEvent::ClearScreen,
    }];
    let raw_event = || -> reedline::ReedlineRawEvent {
        Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .try_into()
            .unwrap()
    };

    let config = shoal_config::Config::default();
    let mut emacs_mode = build_edit_mode(&config, &custom);
    assert_eq!(
        emacs_mode.parse_event(raw_event()),
        ReedlineEvent::ClearScreen
    );

    let mut vi_config = shoal_config::Config::default();
    vi_config.editor.mode = "vi".to_string();
    let mut vi_mode = build_edit_mode(&vi_config, &custom);
    assert_eq!(vi_mode.parse_event(raw_event()), ReedlineEvent::ClearScreen);
}

/// `should_page` (site/content/internals/configuration-reference.md `render.paging`): the four inputs each
/// independently gate paging — disabled, a non-TTY stdout, and output
/// that already fits on one screen must all suppress it regardless of
/// the others; only the conjunction of "enabled + TTY + overflowing"
/// pages.
#[test]
fn should_page_requires_enabled_tty_and_overflowing_output() {
    assert!(should_page(true, true, 100, 24));
    assert!(!should_page(false, true, 100, 24), "paging = \"never\"");
    assert!(!should_page(true, false, 100, 24), "stdout is not a TTY");
    assert!(
        !should_page(true, true, 10, 24),
        "output fits on one screen"
    );
    assert!(
        !should_page(true, true, 24, 24),
        "output exactly filling the screen must not page"
    );
    assert!(
        should_page(true, true, 25, 24),
        "output one line past the screen must page"
    );
}

/// `pager_command` resolution order (site/content/internals/configuration-reference.md): an explicit
/// `render.pager` config command wins over `$PAGER`, which wins over the
/// built-in `less -R` fallback; a blank/whitespace-only value at either
/// layer is treated as unset, not as a literal empty command.
#[test]
fn pager_command_resolution_order() {
    assert_eq!(
        pager_command(Some("bat --paging=always"), Some("more")),
        vec!["bat", "--paging=always"]
    );
    assert_eq!(pager_command(None, Some("most")), vec!["most"]);
    assert_eq!(pager_command(None, None), vec!["less", "-R"]);
    assert_eq!(
        pager_command(Some("   "), Some("most")),
        vec!["most"],
        "a blank config value must fall through to $PAGER"
    );
    assert_eq!(
        pager_command(Some("  "), None),
        vec!["less", "-R"],
        "a blank config value and no $PAGER must fall through to the default"
    );
}

/// `wrapped_line_count`: a short line is one row; a line exactly `width`
/// columns wide is still one row; one column past `width` wraps to a
/// second row; ANSI color escapes (what `render_block` actually emits)
/// contribute zero width so they never inflate the wrap count.
#[test]
fn wrapped_line_count_accounts_for_terminal_wrapping() {
    assert_eq!(wrapped_line_count("short", 80), 1);
    assert_eq!(wrapped_line_count(&"x".repeat(80), 80), 1);
    assert_eq!(wrapped_line_count(&"x".repeat(81), 80), 2);
    assert_eq!(wrapped_line_count(&"x".repeat(160), 80), 2);
    assert_eq!(wrapped_line_count("line1\nline2\nline3", 80), 3);
    assert_eq!(
        wrapped_line_count("\x1b[34;1mkey\x1b[0m  value", 80),
        1,
        "ANSI color escapes must not count toward display width"
    );
    assert_eq!(
        wrapped_line_count(&format!("\x1b[31m{}\x1b[0m", "x".repeat(81)), 80),
        2,
        "a colorized line still wraps by its actual (non-escape) width"
    );
}

/// `render_result_paged` end-to-end gating: with paging disabled
/// (`render.paging = "never"`, the default), even absurdly long output
/// must go straight to a plain print — never touch a pager. This is the
/// "flip the default and nothing else changes" contract the config knob
/// promises; exercised through the real function rather than just
/// `should_page` in isolation.
#[test]
fn render_result_paged_never_pages_when_disabled() {
    let pager = PagerContext {
        enabled: false,
        pager: None,
        configured_width: Some(20),
    };
    let value = Value::Str("x".repeat(10_000));
    // Must not block on a real pager / TTY prompt — disabled short-
    // circuits before any of that is even consulted.
    render_result_paged(&value, false, &pager).unwrap();
}

/// `spawn_pager` end-to-end (site/content/internals/configuration-reference.md: "if you can integration-test
/// the actual pipe cheaply... do it"): spawns a real child process,
/// writes the rendered text through its real stdin pipe, and confirms
/// the bytes actually arrived — the same mechanics `render_result_paged`
/// uses for a real `less`/`bat`, just redirected to a file instead of a
/// TTY so the test needs no terminal at all.
#[test]
fn spawn_pager_pipes_text_through_a_real_child_process() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("captured");
    let argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("cat > {}", out.display()),
    ];
    let ok = spawn_pager(&argv, "hello from the pager test\n");
    assert!(ok, "spawn_pager must report success for a real command");
    let captured = fs::read_to_string(&out).unwrap();
    assert_eq!(captured, "hello from the pager test\n");
}

/// A pager binary that doesn't exist must fail to spawn, cleanly and
/// without panicking, so `render_result_paged` knows to fall back to a
/// plain print rather than silently losing the output.
#[test]
fn spawn_pager_reports_failure_for_a_missing_binary() {
    let argv = vec!["definitely-not-a-real-pager-binary-xyz".to_string()];
    assert!(!spawn_pager(&argv, "irrelevant"));
}

/// The user quitting the pager early (it exits without draining stdin,
/// so the write hits a broken pipe) must be reported as success — not a
/// failure needing a duplicate re-print — and, crucially, must not panic
/// (the SIGPIPE-on-write case the lane brief calls out explicitly).
#[test]
fn spawn_pager_survives_the_reader_quitting_before_draining_stdin() {
    // `true` exits immediately without ever reading its stdin, so the
    // subsequent `write_all` of a large buffer is virtually guaranteed
    // to observe a broken pipe on at least one write.
    let argv = vec!["true".to_string()];
    let big = "x".repeat(1024 * 1024);
    assert!(
        spawn_pager(&argv, &big),
        "a broken pipe must count as handled, not failed"
    );
}

/// Empty argv (a pathological `render.pager = ""` after whitespace
/// trimming somehow reaching this far) must fail closed rather than
/// panic on `argv[0]`.
#[test]
fn spawn_pager_rejects_empty_argv() {
    assert!(!spawn_pager(&[], "text"));
}
