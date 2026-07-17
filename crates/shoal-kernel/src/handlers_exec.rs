//! `dispatch`'s `exec` handler (site/content/internals/language-conformance-contract.md, site/content/internals/kernel-protocol.md): plan/run/
//! approved modes, background tasks, and the synchronous-timeout-becomes-a-
//! task path. Split out of `lib.rs`'s dispatch match (site/content/internals/roadmap-and-priorities.md wave
//! `site/content/internals/change-map.md`; pure mechanical move, zero wire/behavior change.
use super::*;

mod plan;
mod run;
mod task;

impl Kernel {
    pub(crate) fn handle_exec(
        self: &Arc<Self>,
        params: Json,
        client: u64,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        self.ensure_event_owner(&session.key.owner())?;
        let actor = attachment.principal.clone();
        let interactive =
            attachment.tty && attachment.connection_trust == ConnectionTrust::EmbeddedHuman;
        let params: ExecParams = decode(params)?;
        // site/content/internals/kernel-protocol.md: `background:true`, or a synchronous run that
        // exceeds `timeout_ms`, becomes a task ref + events channel —
        // never a blocked context. A bare timeout runs the work on a
        // task and waits up to the deadline for a fast inline answer.
        if params.asynchronous || params.timeout_ms.is_some() {
            return self.handle_exec_task(params, client, attachment, session);
        }
        if params.mode == "plan" {
            return self.handle_exec_plan(params, session, &actor, interactive);
        } else if params.mode != "approved" && params.mode != "run" {
            return Err(RpcError {
                code: INVALID_PARAMS,
                message: "mode must be run or plan".into(),
                data: None,
            });
        }
        self.handle_exec_run(params, attachment, session, actor, interactive)
    }
}

fn parse_ctx_for_kernel(env: &shoal_value::Env, repl: bool) -> shoal_syntax::ParseCtx {
    let mut value_bound = Vec::new();
    let mut cmd_bound = Vec::new();
    for name in env.visible_names() {
        match env.get(&name) {
            Some(value) if value.is_callable() => cmd_bound.push(name),
            Some(_) => value_bound.push(name),
            None => {}
        }
    }
    shoal_syntax::ParseCtx {
        repl,
        value_bound,
        cmd_bound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function_lines(source: &str, signature: &str) -> usize {
        let start = source
            .lines()
            .position(|line| line.contains(signature))
            .unwrap_or_else(|| panic!("missing function signature {signature}"));
        let mut depth = 0isize;
        let mut opened = false;
        for (offset, line) in source.lines().skip(start).enumerate() {
            for byte in line.bytes() {
                match byte {
                    b'{' => {
                        depth += 1;
                        opened = true;
                    }
                    b'}' => depth -= 1,
                    _ => {}
                }
            }
            if opened && depth == 0 {
                return offset + 1;
            }
        }
        panic!("unterminated function {signature}");
    }

    #[test]
    fn exec_production_surfaces_stay_decomposed() {
        let root = include_str!("handlers_exec.rs");
        let root_production = root.split("#[cfg(test)]").next().unwrap();
        let plan = include_str!("handlers_exec/plan.rs");
        let run = include_str!("handlers_exec/run.rs");
        let task = include_str!("handlers_exec/task.rs");

        for (name, source, max_lines) in [
            ("root", root_production, 80),
            ("plan", plan, 140),
            ("run", run, 560),
            ("task", task, 340),
        ] {
            let lines = source.lines().count();
            assert!(
                lines <= max_lines,
                "exec {name} production surface grew to {lines} lines (limit {max_lines}); extract the owning phase"
            );
        }
        for (name, source, signature, max_lines) in [
            ("entry", root, "fn handle_exec(", 45),
            ("plan", plan, "fn handle_exec_plan(", 110),
            ("run", run, "fn handle_exec_run(", 260),
            ("completion", run, "fn complete_exec_value(", 180),
            ("approval", run, "fn claim_exec_approval(", 120),
            ("task", task, "fn handle_exec_task(", 140),
            ("worker", task, "fn run_task_worker(", 180),
        ] {
            let lines = function_lines(source, signature);
            assert!(
                lines <= max_lines,
                "exec {name} function grew to {lines} lines (limit {max_lines}); extract another owned phase"
            );
        }
    }

    fn attached(kernel: &Arc<Kernel>, name: &str) -> (Arc<Session>, Option<Attachment>) {
        let actor = principal();
        let session = kernel.session(name, &actor).expect("create session");
        let attachment = Attachment {
            session: session.clone(),
            principal: actor,
            can_approve: true,
            tty: false,
            cancel_epoch: None,
            bearer: None,
            security_epoch: ATTACH_SECURITY_EPOCH,
            connection_trust: ConnectionTrust::EmbeddedHuman,
        };
        (session, Some(attachment))
    }

    #[test]
    fn journal_poison_is_stable_global_and_restores_approval_reservation() {
        let kernel = Kernel::new();
        kernel.set_allow_self_ack(true);
        let (_session, mut attached) = attached(&kernel, "journal-poison");
        let planned = kernel
            .handle_exec(
                json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
                1,
                &mut attached,
            )
            .unwrap();
        let plan_ref = planned["plan_ref"].as_str().unwrap().to_owned();

        let poisoner = kernel.clone();
        let thread = std::thread::spawn(move || {
            let _journal = poisoner
                .journal
                .lock()
                .expect("test lock should not be poisoned");
            panic!("inject kernel journal poison");
        });
        assert!(thread.join().is_err());

        let approval_error = kernel
            .handle_cap_request(json!({"plan_ref":plan_ref}), &mut attached)
            .expect_err("poisoned journal must reject a durable approval");
        assert_eq!(approval_error.code, INTERNAL_ERROR);
        assert_eq!(approval_error.data.unwrap()["subsystem"], "journal");
        kernel
            .plans
            .transaction(|plans| {
                assert!(matches!(
                    plans.get(&plan_ref).map(|plan| &plan.authorization),
                    Some(PlanAuthorization::PolicyAllowed)
                ));
            })
            .unwrap();

        for _ in 0..2 {
            let error = kernel
                .handle_exec(
                    json!({"src":"1 + 1","mode":"run","position":"value"}),
                    1,
                    &mut attached,
                )
                .expect_err("journal quarantine must remain stable");
            assert_eq!(error.code, INTERNAL_ERROR);
            let data = error.data.unwrap();
            assert_eq!(data["subsystem"], "journal");
            assert_eq!(data["quarantined"], true);
        }

        kernel
            .handle_session_snapshot(&attached)
            .expect("journal poison must not quarantine evaluator session state");
    }

    #[test]
    fn queued_task_installs_its_own_cancellation_epoch_when_it_starts() {
        let kernel = Kernel::new();
        let (session, mut attached) = attached(&kernel, "cancel-epoch-queue");

        // Keep the worker queued after registration, then cancel it and put an
        // unrelated epoch on the evaluator. The worker must replace that
        // unrelated epoch with the token stored in its TaskEntry once it owns
        // the evaluator. The old creation-time reset lost this cancellation.
        let mut evaluator = session
            .evaluator
            .lock()
            .expect("test lock should not be poisoned");
        let background = kernel
            .handle_exec(
                json!({"src":"sh { sleep 30 }", "async":true}),
                1,
                &mut attached,
            )
            .expect("register queued task");
        let task: Ref = serde_json::from_value(background["task"].clone()).unwrap();
        kernel
            .handle_task_cancel(json!({"task":task}), &mut attached)
            .expect("cancel queued task");
        evaluator.set_cancellation_token(shoal_exec::CancelToken::new());
        drop(evaluator);

        let started = Instant::now();
        let record = kernel
            .handle_task_await(json!({"task":task}), &mut attached)
            .expect("await cancelled task");
        assert_eq!(record["state"], "cancelled", "task record: {record}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "pre-cancelled queued task did not stop promptly: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn async_evaluator_poison_finishes_task_as_typed_failure() {
        let kernel = Kernel::new();
        let (session, mut attached) = attached(&kernel, "async-session-poison");
        let poisoner = session.clone();
        let thread = std::thread::spawn(move || {
            let _evaluator = poisoner
                .evaluator
                .lock()
                .expect("test lock should not be poisoned");
            panic!("inject async evaluator poison");
        });
        assert!(thread.join().is_err());

        // Call the handler directly so registration can occur; the worker's
        // recursive dispatch is the boundary that must observe quarantine and
        // turn it into a terminal Task error rather than unwind.
        let background = kernel
            .handle_exec(json!({"src":"1 + 1", "async":true}), 1, &mut attached)
            .expect("poisoned session still registers a worker for this regression");
        let task: Ref = serde_json::from_value(background["task"].clone()).unwrap();
        let record = kernel
            .handle_task_await(json!({"task":task}), &mut attached)
            .expect("task reaches a terminal record");
        assert_eq!(record["state"], "failed");
        assert_eq!(record["error"]["code"], INTERNAL_ERROR);
        assert_eq!(record["error"]["data"]["session_quarantined"], true);
    }

    #[test]
    fn failed_task_launch_releases_resources_even_if_registry_removal_is_unavailable() {
        let kernel = Kernel::new();
        kernel.tasks.configure(1);
        let (session, _) = attached(&kernel, "spawn-cleanup");
        let owner = session.key.owner();
        let active_slot = kernel.tasks.reserve(&owner).unwrap();
        let (_task_id, task_ref) = kernel.tasks.allocate();
        let task = Arc::new(TaskEntry {
            task: task_ref.clone(),
            owner: owner.clone(),
            session_id: session.id.clone(),
            session_lease: Mutex::new(Some(session)),
            started_ns: now_ns(),
            inner: Mutex::new(TaskInner {
                state: "running",
                finished_ns: None,
                result_ref: None,
                exit_code: None,
                error: None,
                active_slot: Some(active_slot),
            }),
            done: Condvar::new(),
            cancel: shoal_exec::CancelToken::new(),
            cancel_requested: AtomicBool::new(false),
        });
        kernel.tasks.insert_checked(task.clone()).unwrap();
        kernel.tasks.poison_entries_for_test();

        kernel.cleanup_failed_task_launch(&task_ref, &task);

        let inner = task.lock_inner().unwrap();
        assert_eq!(inner.state, "failed");
        assert!(inner.active_slot.is_none());
        drop(inner);
        assert!(task.session_lease.lock().unwrap().is_none());
        let replacement = kernel
            .tasks
            .reserve(&owner)
            .expect("direct cleanup must release the one active-task slot");
        drop(replacement);
    }

    #[test]
    fn foreground_exec_starts_a_fresh_cancellation_epoch() {
        let kernel = Kernel::new();
        let (session, mut attached) = attached(&kernel, "cancel-epoch-foreground");
        {
            let evaluator = session
                .evaluator
                .lock()
                .expect("test lock should not be poisoned");
            evaluator.cancel_current();
            assert!(evaluator.cancellation_token().is_cancelled());
        }

        kernel
            .handle_exec(json!({"src":"1 + 2"}), 1, &mut attached)
            .expect("foreground exec after a cancelled epoch");
        assert!(
            !session
                .evaluator
                .lock()
                .expect("test lock should not be poisoned")
                .cancellation_token()
                .is_cancelled(),
            "foreground request inherited the previous cancelled epoch"
        );
    }

    #[test]
    fn transcript_retention_failure_terminalizes_the_coarse_journal_row() {
        let kernel = Kernel::new();
        let (session, mut attached) = attached(&kernel, "transcript-limit");
        {
            let evaluator = session.lock_evaluator().unwrap();
            evaluator
                .env()
                .declare(
                    "out",
                    Value::List(vec![Value::Str("x".repeat(1800 * 1024))]),
                    true,
                )
                .unwrap();
        }
        let src = serde_json::to_string(&"y".repeat(400 * 1024)).unwrap();
        let error = kernel
            .handle_exec(json!({"src":src}), 1, &mut attached)
            .expect_err("the evaluator transcript value wall must reject the append");
        assert_eq!(error.code, RAISED);
        assert_eq!(error.data.unwrap()["code"], "binding_value_limit");

        let rows = kernel
            .journal
            .lock()
            .unwrap()
            .query(&JournalQuery {
                session: Some(session.id.clone()),
                principal: Some(principal()),
                limit: 100,
                ..Default::default()
            })
            .unwrap();
        let row = rows
            .iter()
            .find(|row| row.src == src)
            .expect("coarse request row must remain queryable");
        assert_eq!(row.ok, Some(false));
        assert!(row.dur_ns.is_some(), "failed row must be terminal");
    }

    #[test]
    fn only_embedded_tty_exec_echoes_intermediate_expressions() {
        fn run(kernel: &Arc<Kernel>, name: &str, trust: ConnectionTrust, tty: bool) -> Vec<Value> {
            let (session, mut attached_state) = attached(kernel, name);
            let attachment = attached_state.as_mut().unwrap();
            attachment.connection_trust = trust;
            attachment.tty = tty;
            let captured: Arc<Mutex<Vec<Value>>> = Arc::default();
            let sink = captured.clone();
            session
                .evaluator
                .lock()
                .expect("test lock should not be poisoned")
                .set_statement_sink(Box::new(move |value| {
                    sink.lock()
                        .expect("test lock should not be poisoned")
                        .push(value.clone());
                }));
            kernel
                .handle_exec(json!({"src":"1 + 1\n42"}), 1, &mut attached_state)
                .expect("multi-statement exec");
            captured
                .lock()
                .expect("test lock should not be poisoned")
                .clone()
        }

        let kernel = Kernel::new();
        assert_eq!(
            run(
                &kernel,
                "embedded-tty-echo",
                ConnectionTrust::EmbeddedHuman,
                true,
            ),
            vec![Value::Int(2)]
        );
        assert!(run(&kernel, "public-tty-quiet", ConnectionTrust::Public, true,).is_empty());
        assert!(
            run(
                &kernel,
                "embedded-headless-quiet",
                ConnectionTrust::EmbeddedHuman,
                false,
            )
            .is_empty()
        );
    }

    /// Regression test for the `evaluator.set_source(...)` call added above.
    ///
    /// `Kernel::new()` builds an EPHEMERAL, in-memory-only kernel with no
    /// on-disk state dir at all (`Kernel::state_dir` is `None`), so
    /// `session()` (`crates/shoal-kernel/src/session.rs`) deliberately does
    /// NOT install a journal on this particular session's evaluator — there
    /// is no on-disk store to open one against. (A real on-disk kernel built
    /// via `Kernel::open`/`open_with_policy` DOES get one automatically; see
    /// `kernel_open_installs_a_session_journal_so_history_builtin_sees_real_data`
    /// in `lib.rs`'s test module.) The kernel also always keeps its own
    /// separate exec-level journal (`Kernel::journal`, appended to directly
    /// in `handle_exec` with `src: params.src.clone()`, which was already
    /// correct before this fix and is untouched by it).
    ///
    /// The evaluator's *own* per-statement journal integration
    /// (`journal_begin_stmt`/`stmt_source` in `shoal-eval/src/journal.rs`,
    /// which also backs the in-language `history`/`journal` builtin) only
    /// runs when a journal is installed on the evaluator itself, and
    /// `stmt_source` only has real text to slice once `Evaluator::set_source`
    /// has been called. To observe whether `handle_exec` actually reaches
    /// `set_source` on every code path without needing a real on-disk
    /// kernel, this test installs a journal directly on this ephemeral
    /// session's evaluator purely as a probe, then drives two statements
    /// through the real `handle_exec` entry point: a marker `let`, then
    /// `history`. If `set_source` were never called (the pre-fix state),
    /// `stmt_source` would slice an empty `self.source` and every stmt-level
    /// journal entry's `src` would come back empty; with the fix, the
    /// marker statement's entry carries its exact source text.
    #[test]
    fn exec_calls_set_source_so_stmt_journal_entries_carry_src() {
        // `Kernel::new()`'s default policy (`permissive_policy`) is scoped to
        // THIS process's actual uid principal (`principal()`) — any other
        // principal name gets denied (`leash denied execution`), so the
        // attachment below must use the same principal the kernel treats as
        // permissive rather than an arbitrary test name.
        let actor = principal();
        let kernel = Kernel::new();
        let session = kernel
            .session("set-source-probe", &actor)
            .expect("create session");
        {
            let mut evaluator = session
                .evaluator
                .lock()
                .expect("test lock should not be poisoned");
            evaluator.set_journal(
                Journal::in_memory().expect("in-memory journal"),
                "set-source-probe",
                &actor,
            );
        }
        let mut attached = Some(Attachment {
            session: session.clone(),
            principal: actor,
            can_approve: true,
            tty: false,
            cancel_epoch: None,
            bearer: None,
            security_epoch: ATTACH_SECURITY_EPOCH,
            connection_trust: ConnectionTrust::EmbeddedHuman,
        });

        let marker_src = "let set_source_probe_9182 = 9182";
        let exec = kernel
            .handle_exec(json!({"src": marker_src}), 1, &mut attached)
            .expect("exec of a plain let must succeed");
        assert!(exec.get("ref").is_some(), "exec result: {exec:?}");

        let hist = kernel
            .handle_exec(json!({"src": "history"}), 2, &mut attached)
            .expect("exec of `history` must succeed");
        let cols = hist["value"]["cols"]["src"]
            .as_array()
            .unwrap_or_else(|| panic!("history's table has no src column: {hist:?}"));
        let found = cols
            .iter()
            .find(|v| v["v"] == marker_src)
            .unwrap_or_else(|| {
                panic!("no journal entry with src={marker_src:?} found among {cols:?}")
            });
        assert_eq!(
            found["v"], marker_src,
            "stmt-level journal entry's src must equal the exact submitted source, non-empty \
             (only true once handle_exec calls Evaluator::set_source before eval)"
        );
    }
}
