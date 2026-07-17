//! `dispatch`'s `exec` handler (site/content/internals/language-conformance-contract.md, site/content/internals/kernel-protocol.md): plan/run/
//! approved modes, background tasks, and the synchronous-timeout-becomes-a-
//! task path. Split out of `lib.rs`'s dispatch match (site/content/internals/roadmap-and-priorities.md wave
//! `site/content/internals/change-map.md`; pure mechanical move, zero wire/behavior change.
use super::*;

impl Kernel {
    pub(crate) fn handle_exec(
        self: &Arc<Self>,
        params: Json,
        client: u64,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let actor = attachment.principal.clone();
        let params: ExecParams = decode(params)?;
        // site/content/internals/kernel-protocol.md: `background:true`, or a synchronous run that
        // exceeds `timeout_ms`, becomes a task ref + events channel —
        // never a blocked context. A bare timeout runs the work on a
        // task and waits up to the deadline for a fast inline answer.
        if params.asynchronous || params.timeout_ms.is_some() {
            let elide_spec = params.elide;
            let wait = params.timeout_ms.map(std::time::Duration::from_millis);
            let is_background = params.asynchronous;
            let cancel = {
                let mut evaluator = session.evaluator.lock().unwrap();
                evaluator.reset_cancel();
                evaluator.cancellation_token()
            };
            // site/content/internals/kernel-protocol.md: the events channel is `task.{bare id}`
            // (e.g. `task.7`), NOT `task.{full ref}` (`task.task:7`) — keep
            // the bare numeric id around so the channel name is built from
            // it directly instead of re-deriving it from `task_ref.0` (which
            // is already the `task:N`-prefixed ref string and would double
            // the prefix).
            let task_id = self.next_task.fetch_add(1, Ordering::Relaxed);
            let task_ref = Ref::new("task", task_id);
            let task = Arc::new(TaskEntry {
                task: task_ref.clone(),
                session: session.clone(),
                started_ns: now_ns(),
                inner: Mutex::new(TaskInner {
                    state: "running",
                    finished_ns: None,
                    result_ref: None,
                    error: None,
                }),
                done: Condvar::new(),
                cancel,
                cancel_requested: AtomicBool::new(false),
            });
            self.tasks
                .lock()
                .unwrap()
                .insert(task_ref.clone(), task.clone());
            let waiter = task.clone();
            let kernel = self.clone();
            let mut task_attached = Some(attachment.clone());
            let task_channel = format!("task.{task_id}");
            kernel
                .events
                .publish(&task_channel, json!({"$":"str","v":"started"}));
            std::thread::spawn(move || {
                let response = kernel.dispatch(
                    Request {
                        jsonrpc: JSONRPC.into(),
                        id: Json::Null,
                        method: "exec".into(),
                        params: serde_json::to_value(ExecParams {
                            asynchronous: false,
                            timeout_ms: None,
                            ..params
                        })
                        .unwrap(),
                    },
                    client,
                    &mut task_attached,
                    None,
                );
                let exit_payload;
                {
                    let mut inner = task.inner.lock().unwrap();
                    inner.finished_ns = Some(now_ns());
                    if let Some(error) = response.error {
                        inner.state = if task.cancel_requested.load(Ordering::SeqCst) {
                            "cancelled"
                        } else {
                            "failed"
                        };
                        inner.error = Some(error);
                    } else {
                        inner.result_ref = response
                            .result
                            .as_ref()
                            .and_then(|r| r.get("ref"))
                            .and_then(Json::as_str)
                            .map(|s| Ref(s.into()));
                        // The eval position most callers use for a
                        // background/timed run (`position:"value"`, the MCP
                        // facade's default) captures a failing or
                        // signal-killed outcome as a normal RETURNED value
                        // instead of raising it as an RpcError (site/content/internals/kernel-protocol.md: "a
                        // failed outcome is captured, not raised") — so
                        // `response.error` alone cannot tell a naturally
                        // completed task from one that was killed via
                        // `shoal_cancel`. Inspect the actual outcome the
                        // task produced: a signal-killed outcome while
                        // cancellation was requested is `cancelled`; any
                        // other non-ok outcome is `failed`; only a truly
                        // successful result is `completed`.
                        let outcome = inner
                            .result_ref
                            .as_ref()
                            .and_then(|r| task.session.transcript.lock().unwrap().get(r).cloned());
                        inner.state = match &outcome {
                            Some(Value::Outcome(o)) if !o.ok => {
                                if task.cancel_requested.load(Ordering::SeqCst)
                                    && o.signal.is_some()
                                {
                                    "cancelled"
                                } else {
                                    "failed"
                                }
                            }
                            _ => "completed",
                        };
                    }
                    exit_payload = json!({
                        "$": "record",
                        "v": {
                            "state": {"$":"str","v": inner.state},
                            "ref": inner.result_ref.as_ref()
                                .map(|r| json!({"$":"str","v": r.0}))
                                .unwrap_or(Json::Null),
                        }
                    });
                    task.done.notify_all();
                }
                kernel.events.publish(&task_channel, exit_payload);
            });
            let events_channel = format!("task.{task_id}");
            if is_background {
                return encode(json!({"task":task_ref,"events":events_channel}));
            }
            // Synchronous timeout: wait up to the deadline for the task
            // to finish; return an inline result if it beats the clock,
            // otherwise hand back the still-running task ref.
            let deadline = wait.map(|d| Instant::now() + d);
            let mut inner = waiter.inner.lock().unwrap();
            while matches!(inner.state, "running" | "cancelling") {
                let Some(deadline) = deadline else { break };
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let (guard, timed) = waiter.done.wait_timeout(inner, deadline - now).unwrap();
                inner = guard;
                if timed.timed_out() {
                    break;
                }
            }
            if matches!(inner.state, "running" | "cancelling") {
                drop(inner);
                return encode(json!({"task":task_ref,"events":events_channel,"timed_out":true}));
            }
            let result_ref = inner.result_ref.clone();
            let task_error = inner.error.clone();
            drop(inner);
            if let Some(error) = task_error {
                return Err(error);
            }
            if let Some(result_ref) = result_ref {
                let values = session.transcript.lock().unwrap();
                if let Some(value) = values.get(&result_ref) {
                    let budget = ElideBudget::from_spec(elide_spec.as_ref());
                    let uri = short_ref_to_uri(&result_ref, None);
                    let wire = elide_wire_value(value, &uri, &budget);
                    let render = shoal_value::render::render_block(value, 80);
                    return encode(ExecResult {
                        r#ref: result_ref,
                        value: Some(wire),
                        render: Some(bound_render(render, &uri, !attachment.tty)),
                    });
                }
            }
            return encode(json!({"task":task_ref,"events":events_channel}));
        }
        if params.mode == "plan" {
            let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
                code: PARSE_ERROR,
                message: e.msg,
                data: Some(json!({"span":e.span,"hint":e.hint})),
            })?;
            let ast_json = serde_json::to_string(&ast).map_err(internal)?;
            let plan = {
                let mut evaluator = session.evaluator.lock().unwrap();
                derive_plan(&mut evaluator, &ast, &ast_json)
            };
            let source_hash = source_hash(&params.src);
            let plan_hash = bound_plan_hash(&params.src, &ast_json, &plan, &session.id, &actor);
            let plan_ref = self.allocate_plan_ref(&plan_hash);
            let mut plan = plan;
            plan.plan_ref.clone_from(&plan_ref);
            let verdict = self.policy.evaluate_plan(&actor, &plan);
            let result = PlanResult {
                plan_ref: plan.plan_ref.clone(),
                effects: plan
                    .effects
                    .iter()
                    .map(|e| serde_json::to_value(e).unwrap())
                    .collect(),
                reversibility: reversibility_from_effects(&plan.effects).into(),
                verdict: verdict_name(verdict).into(),
                approval_pending: verdict == Verdict::ApprovalRequired,
            };
            self.plans.lock().unwrap().insert(
                plan.plan_ref.clone(),
                StoredPlan {
                    src: params.src,
                    session: session.id.clone(),
                    principal: actor.clone(),
                    plan_hash,
                    source_hash,
                    plan,
                    approved: verdict == Verdict::Allow,
                    approval: None,
                },
            );
            if verdict == Verdict::ApprovalRequired {
                // site/content/internals/kernel-protocol.md: a plan stuck at `approval_pending` is
                // exactly the moment another principal (a human's session, a
                // supervising agent) needs to learn about it without
                // polling — announce it on `approval` the same way a new
                // transcript value announces on `session.transcript`.
                self.events.publish(
                    "approval",
                    approval_event(&result.plan_ref, &result.effects, &actor),
                );
            }
            return encode(result);
        } else if params.mode == "approved" {
            // "approved" is `plan.apply`'s re-entry, NOT a caller-assertable
            // privilege: without this check any attached principal could send
            // `{"mode":"approved"}` and skip the leash verdict entirely. It
            // must name a stored plan that is approved for THIS
            // session/principal and carries the SAME source.
            let verified = params.plan_ref.as_ref().is_some_and(|r| {
                self.plans.lock().unwrap().get(r).is_some_and(|sp| {
                    sp.session == session.id
                        && sp.principal == actor
                        && sp.src == params.src
                        && (sp.approved
                            || self.policy.evaluate_plan(&actor, &sp.plan) == Verdict::Allow)
                })
            });
            if !verified {
                return Err(RpcError {
                    code: LEASH_DENIED,
                    message: "mode \"approved\" requires an approved plan_ref for this \
                              session/principal (use plan → cap.request → plan.apply)"
                        .into(),
                    data: None,
                });
            }
        } else if params.mode != "run" {
            return Err(RpcError {
                code: INVALID_PARAMS,
                message: "mode must be run or plan".into(),
                data: None,
            });
        }
        let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
            code: PARSE_ERROR,
            message: e.msg,
            data: Some(json!({"span":e.span,"hint":e.hint})),
        })?;
        let ast_json = serde_json::to_string(&ast).map_err(internal)?;
        let mut evaluator = session.evaluator.lock().unwrap();
        // site/content/internals/language-conformance-contract.md leash activation: bind the session's evaluator to this
        // principal's policy so any external spawn resolves and applies
        // an OS sandbox for `actor`. The default-permissive policy
        // resolves to no confinement, so the human path is unchanged.
        evaluator.set_leash_policy(self.policy.clone(), actor.clone());
        let run_plan = derive_plan(&mut evaluator, &ast, &ast_json);
        if params.mode == "approved" {
            let Some(plan_ref) = params.plan_ref.as_ref() else {
                return Err(RpcError {
                    code: LEASH_DENIED,
                    message: "approved execution requires plan_ref".into(),
                    data: None,
                });
            };
            let actual_hash =
                bound_plan_hash(&params.src, &ast_json, &run_plan, &session.id, &actor);
            let plans = self.plans.lock().unwrap();
            let stored = plans.get(plan_ref).ok_or_else(|| RpcError {
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
                    message: "approved plan binding no longer matches source/session/requester"
                        .into(),
                    data: None,
                });
            }
        }
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
        evaluator.interactive = false;
        let started = Instant::now();
        let opaque = run_plan.effects.iter().any(|e| matches!(e, Effect::Opaque));
        let effects_json = serde_json::to_string(&run_plan.effects).map_err(internal)?;
        let entry_id = self
            .journal
            .lock()
            .unwrap()
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
            .map_err(internal)?;
        // HR-D2: an approved re-entry consumes its plan's approval — bind the
        // consuming execution (this journal entry) into the approval record so
        // the requester→plan→approver→scope→execution chain is complete and
        // auditable. `mode:"approved"` only reaches here after the branch above
        // verified `plan_ref` names an approved plan for this session/principal.
        if params.mode == "approved"
            && let Some(plan_ref) = &params.plan_ref
            && let Some(stored) = self.plans.lock().unwrap().get_mut(plan_ref)
            && let Some(approval) = stored.approval.as_mut()
            && approval.plan_ref == stored.plan.plan_ref
            && approval.plan_hash == stored.plan_hash
            && approval.source_hash == stored.source_hash
            && approval.session == stored.session
            && approval.requester == stored.principal
        {
            approval.consumed_by = Some(entry_id);
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
        let value = match eval_with_position(&mut evaluator, &ast, &params.position) {
            Ok(value) => value,
            Err(e) => {
                {
                    let journal = self.journal.lock().unwrap();
                    let _ = journal.finish(entry_id, e.status, false, elapsed_ns(started));
                    if let Some(stderr) = &e.stderr {
                        let _ = journal.record_output(entry_id, "stderr", stderr.as_bytes());
                    }
                }
                self.events.publish_journal(
                    entry_id,
                    journal_event(entry_id, &params.src, false, &actor),
                );
                // site/content/internals/kernel-protocol.md: even a raised error is
                // addressable — store it as an out[n] transcript value
                // so the agent can `shoal_get` the structured error
                // (code/msg/span/hint) instead of parsing message text.
                let value_ref = Ref::new("out", session.next_value.fetch_add(1, Ordering::Relaxed));
                session.transcript.lock().unwrap().insert(
                    value_ref.clone(),
                    Value::Error(std::sync::Arc::new(e.clone())),
                );
                session
                    .client_it
                    .lock()
                    .unwrap()
                    .insert(client, value_ref.clone());
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
        let value_ref = Ref::new("out", session.next_value.fetch_add(1, Ordering::Relaxed));
        session
            .transcript
            .lock()
            .unwrap()
            .insert(value_ref.clone(), value.clone());
        session
            .client_it
            .lock()
            .unwrap()
            .insert(client, value_ref.clone());
        let render = shoal_value::render::render_block(&value, 80);
        // Built once, up front: this SAME payload is both persisted durably
        // (so the `session.transcript` channel can replay it after it ages
        // out of the ring (see `site/content/internals/kernel-protocol.md`) and carried
        // by the live event below. Reconstruction re-wraps the durable copy
        // verbatim rather than re-deriving it from other journal columns.
        let transcript_payload = transcript_event(&value_ref, &value);
        let transcript_ts = now_ns();
        {
            let journal = self.journal.lock().unwrap();
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
        self.events
            .publish_journal(entry_id, journal_event(entry_id, &params.src, true, &actor));
        // site/content/internals/kernel-protocol.md: announce the new transcript value on the
        // `session.transcript` channel — subscribers learn a new
        // out[n] exists (with its shape summary) without polling. Uses
        // `publish_transcript` (not the plain `publish`) so the seq↔entry_id
        // pointer needed for cold replay past the ring is recorded too.
        self.events.publish_transcript(entry_id, transcript_payload);
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
        self.events
            .publish("render", render_event(&value_ref, &bounded_render));
        encode(ExecResult {
            r#ref: value_ref,
            value: Some(elide_wire_value(&value, &exec_uri, &exec_budget)),
            render: Some(bounded_render),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let mut evaluator = session.evaluator.lock().unwrap();
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
