//! Background execution and synchronous timeout task lifecycle.

use super::*;

impl Kernel {
    pub(super) fn handle_exec_task(
        self: &Arc<Self>,
        params: ExecParams,
        client: u64,
        attachment: &Attachment,
        session: &Arc<Session>,
    ) -> Result<Json, RpcError> {
        let active_slot = self.tasks.reserve(&session.key.owner())?;
        let elide_spec = params.elide;
        let wait = params.timeout_ms.map(std::time::Duration::from_millis);
        let is_background = params.asynchronous;
        // The worker may queue behind another execution on this session's
        // evaluator. Keep its epoch request-local until the worker owns
        // the evaluator; installing it here would let the next request
        // clobber it before this task starts.
        let cancel = shoal_exec::CancelToken::new();
        // site/content/internals/kernel-protocol.md: the events channel is `task.{bare id}`
        // (e.g. `task.7`), NOT `task.{full ref}` (`task.task:7`) — keep
        // the bare numeric id around so the channel name is built from
        // it directly instead of re-deriving it from `task_ref.0` (which
        // is already the `task:N`-prefixed ref string and would double
        // the prefix).
        let (task_id, task_ref) = self.tasks.allocate();
        let task_owner = session.key.owner();
        let task = Arc::new(TaskEntry {
            task: task_ref.clone(),
            owner: task_owner.clone(),
            session_id: session.id.clone(),
            session_lease: Mutex::new(Some(session.clone())),
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
            cancel: cancel.clone(),
            cancel_requested: AtomicBool::new(false),
        });
        self.tasks.insert_checked(task.clone())?;
        let waiter = task.clone();
        let worker_session = session.clone();
        let kernel = self.clone();
        let mut task_attachment = attachment.clone();
        task_attachment.cancel_epoch = Some(cancel.clone());
        let task_attached = Some(task_attachment);
        let task_channel = format!("task.{task_id}");
        kernel.events.publish(
            &session.key.owner(),
            &task_channel,
            json!({"$":"str","v":"started"}),
        );
        let spawn_result = std::thread::Builder::new()
            .name(format!("shoal-task-{task_id}"))
            .spawn(move || {
                run_task_worker(
                    kernel,
                    task,
                    task_channel,
                    worker_session,
                    params,
                    client,
                    task_attached,
                );
            });
        if let Err(error) = spawn_result {
            // Release the permit and Session lease through the owned task even
            // if a simultaneous registry failure makes removal unavailable.
            waiter.fail_worker_panic();
            self.tasks.remove(&task_ref);
            return Err(internal(error));
        }
        let events_channel = format!("task.{task_id}");
        if is_background {
            return encode(json!({"task":task_ref,"events":events_channel}));
        }
        // Synchronous timeout: wait up to the deadline for the task
        // to finish; return an inline result if it beats the clock,
        // otherwise hand back the still-running task ref.
        let deadline = wait.map(|d| Instant::now() + d);
        let mut inner = waiter.lock_inner()?;
        while matches!(inner.state, "running" | "cancelling") {
            let Some(deadline) = deadline else { break };
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (guard, timed) = match waiter.done.wait_timeout(inner, deadline - now) {
                Ok(result) => result,
                Err(poisoned) => return Err(waiter.repair_timeout_wait_poison(poisoned)),
            };
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
        let exit_code = inner.exit_code;
        let task_error = inner.error.clone();
        drop(inner);
        if let Some(error) = task_error {
            return Err(error);
        }
        if let Some(result_ref) = result_ref {
            let values = session.lock_transcript()?;
            if let Some(value) = values.get(&result_ref) {
                let budget = ElideBudget::from_spec(elide_spec.as_ref());
                let uri = short_ref_to_uri(&result_ref, None);
                let wire = elide_wire_value(value, &uri, &budget);
                let render = shoal_value::render::render_block(value, 80);
                return encode(ExecResult {
                    r#ref: result_ref,
                    value: Some(wire),
                    render: Some(bound_render(render, &uri, !attachment.tty)),
                    exit_code,
                });
            }
        }
        encode(json!({"task":task_ref,"events":events_channel}))
    }
}

fn run_task_worker(
    kernel: Arc<Kernel>,
    task: Arc<TaskEntry>,
    task_channel: String,
    worker_session: Arc<Session>,
    params: ExecParams,
    client: u64,
    mut task_attached: Option<Attachment>,
) {
    let task_trust = task_attached
        .as_ref()
        .map_or(ConnectionTrust::Public, |attachment| {
            attachment.connection_trust
        });
    let mut worker_guard = TaskWorkerGuard::new(task.clone(), kernel.clone(), task_channel.clone());
    let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        kernel.dispatch(
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
            task_trust,
        )
    }))
    .unwrap_or_else(|_| Response {
        jsonrpc: JSONRPC.into(),
        id: Json::Null,
        result: None,
        error: Some(RpcError {
            code: INTERNAL_ERROR,
            message: "task worker panicked".into(),
            data: None,
        }),
    });
    let response_result_ref = response
        .result
        .as_ref()
        .and_then(|result| result.get("ref"))
        .and_then(Json::as_str)
        .map(|value| Ref(value.into()));
    let outcome_result = if response.error.is_none() {
        match response_result_ref.as_ref() {
            Some(result_ref) => worker_session
                .lock_transcript()
                .map(|values| values.get(result_ref).cloned()),
            None => Ok(None),
        }
    } else {
        Ok(None)
    };
    let (outcome, transcript_error) = match outcome_result {
        Ok(outcome) => (outcome, None),
        Err(error) => (None, Some(error)),
    };
    let exit_payload;
    let active_slot;
    let notify_completion;
    {
        let mut inner = match task.lock_inner() {
            Ok(inner) => inner,
            Err(_) => {
                kernel.events.publish(
                    &task.owner,
                    &task_channel,
                    json!({
                        "$": "record",
                        "v": {
                            "state": {"$":"str", "v":"failed"},
                            "ref": Json::Null,
                        }
                    }),
                );
                kernel.reap_finished_tasks(&task.owner);
                worker_guard.disarm();
                return;
            }
        };
        notify_completion = inner.finished_ns.is_none();
        if !notify_completion {
            // A request/waiter already reconstructed this task
            // as terminal after observing poison. Preserve that
            // failure instead of letting a late worker overwrite
            // it with an apparently successful result.
        } else if let Some(error) = response.error {
            inner.finished_ns = Some(now_ns());
            inner.state = if task.cancel_requested.load(Ordering::SeqCst) {
                "cancelled"
            } else {
                "failed"
            };
            inner.error = Some(error);
        } else if let Some(error) = transcript_error {
            inner.finished_ns = Some(now_ns());
            inner.state = "failed";
            inner.error = Some(error);
        } else {
            inner.finished_ns = Some(now_ns());
            inner.exit_code = response
                .result
                .as_ref()
                .and_then(|result| result.get("exit_code"))
                .and_then(Json::as_i64)
                .and_then(|code| i32::try_from(code).ok());
            inner.result_ref = response_result_ref;
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
            inner.state = match &outcome {
                Some(Value::Outcome(o)) if !o.ok => {
                    if task.cancel_requested.load(Ordering::SeqCst) && o.signal.is_some() {
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
        active_slot = notify_completion
            .then(|| inner.active_slot.take())
            .flatten();
    }
    if notify_completion {
        drop(active_slot);
        task.release_session_lease();
        task.done.notify_all();
    }
    worker_guard.disarm();
    kernel
        .events
        .publish(&task.owner, &task_channel, exit_payload);
    kernel.reap_finished_tasks(&task.owner);
}
