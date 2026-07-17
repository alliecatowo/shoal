//! `dispatch`'s `exec` handler (site/content/internals/language-conformance-contract.md, site/content/internals/kernel-protocol.md): plan/run/
//! approved modes, background tasks, and the synchronous-timeout-becomes-a-
//! task path. Split out of `lib.rs`'s dispatch match (site/content/internals/roadmap-and-priorities.md wave
//! `site/content/internals/change-map.md`; pure mechanical move, zero wire/behavior change.
use super::*;

mod plan;
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
        // Parsing is session-stateful in a REPL: persisted value bindings,
        // functions, `it`, and `out` determine command-vs-expression
        // dispatch. Hold the evaluator lock from context construction through
        // evaluation so an async worker cannot parse against a stale Env.
        let mut evaluator = session.lock_evaluator()?;
        let mut ast = shoal_syntax::parse_with_ctx(
            &params.src,
            parse_ctx_for_kernel(evaluator.env(), interactive),
        )
        .map_err(|e| RpcError {
            code: PARSE_ERROR,
            message: e.msg,
            data: Some(json!({"span":e.span,"hint":e.hint})),
        })?;
        session.rewrite_out_undo(&mut ast);
        let ast_json = serde_json::to_string(&ast).map_err(internal)?;
        if let Some(cancel) = attachment.cancel_epoch.clone() {
            evaluator.set_cancellation_token(cancel);
        } else {
            // Foreground requests own distinct epochs too. In particular, a
            // cancelled background task must not leave the next foreground
            // command pre-cancelled.
            evaluator.reset_cancel();
        }
        // site/content/internals/language-conformance-contract.md leash activation: bind the session's evaluator to this
        // principal's policy so any external spawn resolves and applies
        // an OS sandbox for `actor`. The default-permissive policy
        // resolves to no confinement, so the human path is unchanged.
        evaluator.set_leash_policy(self.policy.clone(), actor.clone());
        let run_plan = derive_plan(&mut evaluator, &ast, &ast_json);
        let claimed_approval = if params.mode == "approved" {
            let Some(plan_ref) = params.plan_ref.as_ref() else {
                return Err(RpcError {
                    code: LEASH_DENIED,
                    message: "approved execution requires plan_ref".into(),
                    data: None,
                });
            };
            let actual_hash =
                bound_plan_hash(&params.src, &ast_json, &run_plan, &session.id, &actor);
            self.plans
                .transaction(|plans| -> Result<Option<ApprovalRecord>, RpcError> {
                    if plans.get(plan_ref).is_some_and(plan_expired) {
                        plans.remove(plan_ref);
                        return Err(RpcError {
                            code: UNKNOWN_PLAN,
                            message: "unknown or expired plan_ref".into(),
                            data: Some(json!({"plan_ref": plan_ref})),
                        });
                    }
                    let stored = plans.get_mut(plan_ref).ok_or_else(|| RpcError {
                        code: UNKNOWN_PLAN,
                        message: "unknown plan_ref".into(),
                        data: None,
                    })?;
                    if stored.plan_hash != actual_hash
                        || stored.source_hash != source_hash(&params.src)
                        || stored.session != session.id
                        || stored.principal != actor
                    {
                        return Err(RpcError {
                            code: LEASH_DENIED,
                            message:
                                "approved plan binding no longer matches source/session/requester"
                                    .into(),
                            data: None,
                        });
                    }
                    let claimed = match &stored.authorization {
                        PlanAuthorization::PolicyAllowed
                            if self.policy.evaluate_plan(&actor, &stored.plan)
                                == Verdict::Allow =>
                        {
                            None
                        }
                        PlanAuthorization::Approved(record) => {
                            let record = record.clone();
                            stored.authorization = PlanAuthorization::Claimed(record.clone());
                            Some(record)
                        }
                        PlanAuthorization::Claimed(_) => {
                            return Err(RpcError {
                                code: LEASH_DENIED,
                                message: "approved plan is already being applied".into(),
                                data: Some(json!({"plan_ref": plan_ref})),
                            });
                        }
                        PlanAuthorization::Granting { .. } => {
                            return Err(RpcError {
                                code: LEASH_DENIED,
                                message: "approval grant is still being durably recorded".into(),
                                data: Some(json!({"plan_ref": plan_ref})),
                            });
                        }
                        PlanAuthorization::Consumed(record) => {
                            return Err(RpcError {
                                code: LEASH_DENIED,
                                message: "approval was already consumed".into(),
                                data: Some(json!({
                                    "plan_ref": plan_ref,
                                    "consumed_by": record.consumed_by,
                                })),
                            });
                        }
                        PlanAuthorization::Pending => {
                            return Err(RpcError {
                                code: LEASH_DENIED,
                                message: "mode \"approved\" requires a granted approval".into(),
                                data: Some(json!({"plan_ref": plan_ref})),
                            });
                        }
                        PlanAuthorization::Denied | PlanAuthorization::PolicyAllowed => {
                            return Err(RpcError {
                                code: LEASH_DENIED,
                                message: "plan is not authorized for approved execution".into(),
                                data: Some(json!({"plan_ref": plan_ref})),
                            });
                        }
                    };
                    Ok(claimed)
                })??
        } else {
            None
        };
        if params.mode == "run" {
            match self.policy.evaluate_plan(&actor, &run_plan) {
                Verdict::Deny => {
                    return Err(RpcError {
                        code: LEASH_DENIED,
                        message: "leash denied execution".into(),
                        data: Some(json!({"effects":run_plan.effects})),
                    });
                }
                Verdict::ApprovalRequired => {
                    return Err(RpcError {
                        code: APPROVAL_REQUIRED,
                        message: "approval required; plan first".into(),
                        data: Some(json!({"effects":run_plan.effects})),
                    });
                }
                Verdict::Allow => {}
            }
        }
        evaluator.set_interactive(interactive);
        evaluator.set_echo_mode(if interactive {
            EchoMode::All
        } else {
            EchoMode::Quiet
        });
        let started = Instant::now();
        let opaque = run_plan.effects.iter().any(|e| matches!(e, Effect::Opaque));
        let mut journal_effects = run_plan
            .effects
            .iter()
            .map(|effect| serde_json::to_value(effect).map_err(internal))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(approval) = &claimed_approval {
            journal_effects.push(json!({
                "kind": "approval.consume",
                "plan_ref": approval.plan_ref,
                "plan_hash": approval.plan_hash,
                "source_hash": approval.source_hash,
                "requester": approval.requester,
                "approver": approval.approver,
                "scope": approval.scope,
                "grant_audit_id": approval.grant_audit_id,
            }));
        }
        let effects_json = serde_json::to_string(&journal_effects).map_err(internal)?;
        let append_result = self
            .journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))
            .and_then(|journal| {
                journal
                    .append(&EntryRecord {
                        session: session.id.clone(),
                        // Cloned, not moved: both the error and success paths below
                        // publish a `journal` event (site/content/internals/kernel-protocol.md) carrying this
                        // same principal, well after this record is built.
                        principal: actor.clone(),
                        ts_ns: now_ns(),
                        cwd: evaluator.cwd().as_os_str().as_bytes().to_vec(),
                        src: params.src.clone(),
                        ast_json: ast_json.clone(),
                        effects_json,
                        opaque,
                    })
                    .map_err(internal)
            });
        let entry_id = match append_result {
            Ok(entry_id) => entry_id,
            Err(error) => {
                if let (Some(plan_ref), Some(approval)) = (&params.plan_ref, &claimed_approval) {
                    let _ = self.plans.transaction(|plans| {
                        if let Some(stored) = plans.get_mut(plan_ref)
                            && matches!(
                                &stored.authorization,
                                PlanAuthorization::Claimed(current) if current == approval
                            )
                        {
                            stored.authorization = PlanAuthorization::Approved(approval.clone());
                        }
                    });
                }
                return Err(error);
            }
        };
        if let (Some(plan_ref), Some(approval)) = (&params.plan_ref, claimed_approval) {
            let consumed = self.plans.transaction(|plans| {
                let Some(stored) = plans.get_mut(plan_ref) else {
                    return Err("claimed plan disappeared before execution");
                };
                if !matches!(
                    &stored.authorization,
                    PlanAuthorization::Claimed(current) if current == &approval
                ) {
                    return Err("claimed approval changed before execution");
                }
                let mut consumed = approval;
                consumed.consumed_by = Some(entry_id);
                stored.authorization = PlanAuthorization::Consumed(consumed);
                Ok(())
            });
            let consumed = match consumed {
                Ok(consumed) => consumed,
                Err(error) => {
                    let journal = self
                        .journal
                        .lock()
                        .map_err(|_| poisoned_subsystem("journal"))?;
                    let _ = journal.finish(entry_id, None, false, 0);
                    return Err(error);
                }
            };
            if let Err(message) = consumed {
                let journal = self
                    .journal
                    .lock()
                    .map_err(|_| poisoned_subsystem("journal"))?;
                let _ = journal.finish(entry_id, None, false, 0);
                return Err(internal(message));
            }
        }
        // Hand the evaluator this call's source so each journaled top-level
        // statement can slice its own `src` (site/content/internals/language-conformance-contract.md) — mirrors the REPL's fix
        // at `crates/shoal/src/repl.rs` (`evaluator.set_source(run_src...)`
        // right before `eval_program`): without this, `stmt_source` has
        // nothing to slice from, so the evaluator's own per-statement journal
        // entries (and the `history`/`journal` builtin backed by them) show an
        // empty `src` column for every kernel-hosted statement. Set right
        // before eval, on the session's evaluator, under the same lock this
        // whole `run`/`approved` path already holds — covers both modes (the
        // "approved" branch above falls through to this same code, and the
        // async/timeout wrapper above re-enters `handle_exec` with the same
        // `src` via `dispatch`, hitting this exact call again).
        evaluator.set_source(params.src.clone());
        let evaluator_started_ns = now_ns();
        let value = match eval_with_position(&mut evaluator, &ast, &params.position) {
            Ok(value) => value,
            Err(e) => {
                let row_finished = {
                    let journal = self
                        .journal
                        .lock()
                        .map_err(|_| poisoned_subsystem("journal"))?;
                    let finished = journal
                        .finish(entry_id, e.status, false, elapsed_ns(started))
                        .is_ok();
                    if finished && let Some(stderr) = &e.stderr {
                        let _ = journal.record_output(entry_id, "stderr", stderr.as_bytes());
                    }
                    finished
                };
                if row_finished {
                    self.events.publish_journal(
                        &session.key.owner(),
                        entry_id,
                        journal_event(entry_id, &params.src, false, &actor),
                    );
                }
                // site/content/internals/kernel-protocol.md: even a raised error is
                // addressable — store it as an out[n] transcript value
                // so the agent can `shoal_get` the structured error
                // (code/msg/span/hint) instead of parsing message text.
                let value_ref = Ref::new("out", session.next_value.fetch_add(1, Ordering::Relaxed));
                session.insert_transcript(
                    value_ref.clone(),
                    Value::Error(std::sync::Arc::new(e.clone())),
                );
                let uri = short_ref_to_uri(&value_ref, None);
                return Err(RpcError {
                    code: RAISED,
                    message: e.msg,
                    data: Some(json!({
                        "code": e.code, "span": e.span, "hint": e.hint,
                        "status": e.status, "stderr": e.stderr,
                        "ref": value_ref, "uri": uri
                    })),
                });
            }
        };
        let exit_code = evaluator.take_exit();
        // Keep the evaluator-visible REPL transcript (`it` and `out`) in
        // lockstep with the kernel's addressable Session transcript. Failed
        // evaluations intentionally do not reach this point, matching the
        // standalone REPL's successful-value-only contract.
        if let Err(e) = evaluator.record_transcript(&value) {
            return Err(RpcError {
                code: RAISED,
                message: e.msg,
                data: Some(json!({
                    "code": e.code, "span": e.span, "hint": e.hint,
                    "status": e.status, "stderr": e.stderr
                })),
            });
        }
        let evaluator_entry_id = self
            .journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))?
            .query(&JournalQuery {
                since_ts_ns: Some(evaluator_started_ns),
                session: Some(session.id.clone()),
                principal: Some(actor.clone()),
                ok: Some(true),
                limit: 1,
                ..Default::default()
            })
            .ok()
            .and_then(|rows| rows.first().map(|row| row.id));
        session.push_out_entry(evaluator_entry_id);
        let value_ref = Ref::new("out", session.next_value.fetch_add(1, Ordering::Relaxed));
        if let Err(error) = session.insert_transcript_checked(value_ref.clone(), value.clone()) {
            let journal = self
                .journal
                .lock()
                .map_err(|_| poisoned_subsystem("journal"))?;
            let _ = journal.finish(entry_id, None, false, elapsed_ns(started));
            return Err(error);
        }
        let render = shoal_value::render::render_block(&value, 80);
        // Built once, up front: this SAME payload is both persisted durably
        // (so the `session.transcript` channel can replay it after it ages
        // out of the ring (see `site/content/internals/kernel-protocol.md`) and carried
        // by the live event below. Reconstruction re-wraps the durable copy
        // verbatim rather than re-deriving it from other journal columns.
        let transcript_payload = transcript_event(&value_ref, &value);
        let transcript_ts = now_ns();
        {
            let journal = self
                .journal
                .lock()
                .map_err(|_| poisoned_subsystem("journal"))?;
            journal
                .finish(entry_id, Some(0), true, elapsed_ns(started))
                .map_err(internal)?;
            journal
                .record_output(
                    entry_id,
                    "value",
                    &serde_json::to_vec(&wire_value(&value)).map_err(internal)?,
                )
                .map_err(internal)?;
            if !render.is_empty() {
                journal
                    .record_output(entry_id, "render", render.as_bytes())
                    .map_err(internal)?;
            }
            if let Value::Outcome(out) = &value {
                journal
                    .record_output(entry_id, "stdout", &out.stdout)
                    .map_err(internal)?;
                if !out.stderr.is_empty() {
                    journal
                        .record_output(entry_id, "stderr", &out.stderr)
                        .map_err(internal)?;
                }
            }
            journal
                .record_transcript_event(
                    entry_id,
                    transcript_ts,
                    &serde_json::to_string(&transcript_payload).map_err(internal)?,
                )
                .map_err(internal)?;
        }
        self.events.publish_journal(
            &session.key.owner(),
            entry_id,
            journal_event(entry_id, &params.src, true, &actor),
        );
        // site/content/internals/kernel-protocol.md: announce the new transcript value on the
        // `session.transcript` channel — subscribers learn a new
        // out[n] exists (with its shape summary) without polling. Uses
        // `publish_transcript` (not the plain `publish`) so the seq↔entry_id
        // pointer needed for cold replay past the ring is recorded too.
        self.events
            .publish_transcript(&session.key.owner(), entry_id, transcript_payload);
        let exec_budget = ElideBudget::from_spec(params.elide.as_ref());
        let exec_uri = short_ref_to_uri(&value_ref, None);
        // The journal keeps the full render above (record_output); the wire
        // response bounds it to the same hard cap as MCP's content[0].text
        // (site/content/internals/kernel-protocol.md) — a huge render must never bypass the wall the
        // structured value already respects.
        let bounded_render = bound_render(render, &exec_uri, !attachment.tty);
        // site/content/internals/kernel-protocol.md: a live UI subscribing to `render` sees the same
        // string the exec response itself carries — no separate unbounded
        // copy, no polling `value.get {format:"render"}`.
        self.events.publish(
            &session.key.owner(),
            "render",
            render_event(&value_ref, &bounded_render),
        );
        encode(ExecResult {
            r#ref: value_ref,
            value: Some(elide_wire_value(&value, &exec_uri, &exec_budget)),
            render: Some(bounded_render),
            exit_code,
        })
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
