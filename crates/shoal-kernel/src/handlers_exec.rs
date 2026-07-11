//! `dispatch`'s `exec` handler (TDD §10, AGENT-SURFACE §5/§8): plan/run/
//! approved modes, background tasks, and the synchronous-timeout-becomes-a-
//! task path. Split out of `lib.rs`'s dispatch match (docs/ROADMAP.md wave
//! R4): pure mechanical move, zero wire/behavior change.
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
        // AGENT-SURFACE §5: `background:true`, or a synchronous run that
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
            let task_ref = Ref::new("task", self.next_task.fetch_add(1, Ordering::Relaxed));
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
            let task_channel = format!("task.{}", task_ref.0);
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
                        inner.state = "completed";
                        inner.result_ref = response
                            .result
                            .as_ref()
                            .and_then(|r| r.get("ref"))
                            .and_then(Json::as_str)
                            .map(|s| Ref(s.into()));
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
            let events_channel = format!("task.{}", task_ref.0);
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
                        render: Some(render),
                    });
                }
            }
            return encode(json!({"task":task_ref,"events":events_channel}));
        }
        if params.mode == "plan" {
            let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
                code: -32001,
                message: e.msg,
                data: Some(json!({"span":e.span,"hint":e.hint})),
            })?;
            let ast_json = serde_json::to_string(&ast).map_err(internal)?;
            let plan = {
                let mut evaluator = session.evaluator.lock().unwrap();
                derive_plan(&mut evaluator, &ast, &ast_json)
            };
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
                    plan,
                    approved: verdict == Verdict::Allow,
                },
            );
            return encode(result);
        } else if params.mode != "run" && params.mode != "approved" {
            return Err(RpcError {
                code: -32602,
                message: "mode must be run or plan".into(),
                data: None,
            });
        }
        let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
            code: -32001,
            message: e.msg,
            data: Some(json!({"span":e.span,"hint":e.hint})),
        })?;
        let ast_json = serde_json::to_string(&ast).map_err(internal)?;
        let mut evaluator = session.evaluator.lock().unwrap();
        // TDD §8 leash activation: bind the session's evaluator to this
        // principal's policy so any external spawn resolves and applies
        // an OS sandbox for `actor`. The default-permissive policy
        // resolves to no confinement, so the human path is unchanged.
        evaluator.set_leash_policy(self.policy.clone(), actor.clone());
        let run_plan = derive_plan(&mut evaluator, &ast, &ast_json);
        if params.mode == "run" {
            match self.policy.evaluate_plan(&actor, &run_plan) {
                Verdict::Deny => {
                    return Err(RpcError {
                        code: -32010,
                        message: "leash denied execution".into(),
                        data: Some(json!({"effects":run_plan.effects})),
                    });
                }
                Verdict::ApprovalRequired => {
                    return Err(RpcError {
                        code: -32011,
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
                principal: actor,
                ts_ns: now_ns(),
                cwd: evaluator.cwd().as_os_str().as_bytes().to_vec(),
                src: params.src.clone(),
                ast_json: ast_json.clone(),
                effects_json,
                opaque,
            })
            .map_err(internal)?;
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
                // AGENT-SURFACE §0/§5: even a raised error is
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
                    code: -32002,
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
        }
        // AGENT-SURFACE §4: announce the new transcript value on the
        // `session.transcript` channel — subscribers learn a new
        // out[n] exists (with its shape summary) without polling.
        self.events
            .publish("session.transcript", transcript_event(&value_ref, &value));
        let exec_budget = ElideBudget::from_spec(params.elide.as_ref());
        let exec_uri = short_ref_to_uri(&value_ref, None);
        encode(ExecResult {
            r#ref: value_ref,
            value: Some(elide_wire_value(&value, &exec_uri, &exec_budget)),
            render: Some(render),
        })
    }
}
