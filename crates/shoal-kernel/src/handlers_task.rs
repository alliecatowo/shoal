//! `dispatch` handlers for task lifecycle and plan approval: `task.list`,
//! `task.get`, `task.await`, `task.cancel`, `task.suspend`, `task.resume`,
//! `plan.apply`, `cap.request`. Split out of `lib.rs`'s dispatch match
//! (docs/ROADMAP.md wave R4): pure mechanical move, zero wire/behavior change.
use super::*;

impl Kernel {
    pub(crate) fn handle_task_list(
        self: &Arc<Self>,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let records: Vec<_> = self
            .tasks
            .lock()
            .unwrap()
            .values()
            .filter(|task| task.session.id == session.id)
            .map(task_record)
            .collect();
        encode(records)
    }

    pub(crate) fn handle_task_get(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let p: TaskParams = decode(params)?;
        let task = self.task(&p.task)?;
        if task.session.id != session.id {
            return Err(RpcError {
                code: -32021,
                message: "unknown task ref".into(),
                data: None,
            });
        }
        // Non-blocking snapshot (unlike task.await): the current record.
        encode(task_record(&task))
    }

    pub(crate) fn handle_task_await(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let p: TaskParams = decode(params)?;
        let task = self.task(&p.task)?;
        if task.session.id != session.id {
            return Err(RpcError {
                code: -32021,
                message: "unknown task ref".into(),
                data: None,
            });
        }
        let mut inner = task.inner.lock().unwrap();
        while matches!(inner.state, "running" | "cancelling") {
            inner = task.done.wait(inner).unwrap();
        }
        encode(task_record_locked(&task, &inner))
    }

    pub(crate) fn handle_task_cancel(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let p: TaskParams = decode(params)?;
        let task = self.task(&p.task)?;
        if task.session.id != session.id {
            return Err(RpcError {
                code: -32021,
                message: "unknown task ref".into(),
                data: None,
            });
        }
        task.cancel_requested.store(true, Ordering::SeqCst);
        {
            let mut inner = task.inner.lock().unwrap();
            if inner.state == "running" {
                inner.state = "cancelling";
            }
        }
        task.cancel.cancel();
        encode(json!({"task":p.task,"cancel_requested":true}))
    }

    pub(crate) fn handle_task_suspend(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let p: TaskParams = decode(params)?;
        let task = self.task(&p.task)?;
        if task.session.id != session.id {
            return Err(RpcError {
                code: -32021,
                message: "unknown task ref".into(),
                data: None,
            });
        }
        Err(RpcError {
            code: -32020,
            message: "task suspension is unavailable for evaluator-owned processes".into(),
            data: Some(json!({"task":p.task})),
        })
    }

    // docs/ROADMAP.md R3: added alongside the pre-existing
    // `task.suspend` above, honest in the same way — a kernel task is
    // a Rust thread recursively calling back into `dispatch`, not a
    // single tracked child process/group, so there is nothing here to
    // send `SIGCONT` to yet. Real suspend/resume for a task's spawned
    // children lands with the eval sibling's task-lifecycle methods
    // (`.suspend()`/`.resume()`, R3); once a task's process handle is
    // reachable from here, this stub becomes the real thing without
    // changing the wire shape.
    pub(crate) fn handle_task_resume(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let p: TaskParams = decode(params)?;
        let task = self.task(&p.task)?;
        if task.session.id != session.id {
            return Err(RpcError {
                code: -32021,
                message: "unknown task ref".into(),
                data: None,
            });
        }
        Err(RpcError {
            code: -32020,
            message: "task resume is unavailable for evaluator-owned processes".into(),
            data: Some(json!({"task":p.task})),
        })
    }

    /// `plan.get` (AGENT-SURFACE §1, `shoal://plan/{ref}`): the stored plan a
    /// prior `exec {mode:"plan"}` / `shoal_plan` derived and keyed by its
    /// `plan:<hex16>` ref — its canonical AST (re-parsed from the stored
    /// source), concrete effects, reversibility, and the *current* leash
    /// verdict, mirroring what `explain` returns plus the verdict fields
    /// `exec`'s plan mode reports. Session/principal-scoped like `plan.apply`
    /// (a plan is private to the principal that derived it), and an unknown or
    /// expired ref is a clear not-found (`-32012`), never a silent empty plan.
    pub(crate) fn handle_plan_get(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let p: PlanApplyParams = decode(params)?;
        let plans = self.plans.lock().unwrap();
        let stored = plans.get(&p.plan_ref).ok_or_else(|| RpcError {
            code: -32012,
            message: "unknown or expired plan_ref".into(),
            data: Some(json!({ "plan_ref": p.plan_ref })),
        })?;
        if stored.session != session.id || stored.principal != attachment.principal {
            return Err(RpcError {
                code: -32010,
                message: "plan belongs to another principal/session".into(),
                data: None,
            });
        }
        // Re-parse the stored source so the canonical AST travels alongside the
        // derived effects (the plan record itself keeps only source + effects).
        // The source parsed cleanly when the plan was stored, so this succeeds;
        // an `ast: null` is an honest gap, never a fabricated tree.
        let ast = shoal_syntax::parse(&stored.src).ok();
        let verdict = self.policy.evaluate_plan(&stored.principal, &stored.plan);
        encode(json!({
            "ast_version": AST_VERSION,
            "ast": ast,
            "plan_ref": stored.plan.plan_ref,
            "effects": stored.plan.effects,
            "reversibility": reversibility_from_effects(&stored.plan.effects),
            "verdict": verdict_name(verdict),
            "approval_pending": verdict == Verdict::ApprovalRequired,
            "approved": stored.approved,
            "src": stored.src,
        }))
    }

    /// `plan.list` (AGENT-SURFACE §8): the open plans this session/principal
    /// derived and can inspect (`plan.get`) or apply (`plan.apply`) — the
    /// enumerable backing for `shoal://plan/*` in `resources/list`. Scoped the
    /// same way `plan.get`/`plan.apply` are: a principal only ever sees its own
    /// plans.
    pub(crate) fn handle_plan_list(
        self: &Arc<Self>,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let plans = self.plans.lock().unwrap();
        let records: Vec<Json> = plans
            .values()
            .filter(|sp| sp.session == session.id && sp.principal == attachment.principal)
            .map(|sp| {
                let verdict = self.policy.evaluate_plan(&sp.principal, &sp.plan);
                json!({
                    "plan_ref": sp.plan.plan_ref,
                    "effects": sp.plan.effects,
                    "reversibility": reversibility_from_effects(&sp.plan.effects),
                    "verdict": verdict_name(verdict),
                    "approval_pending": verdict == Verdict::ApprovalRequired,
                    "approved": sp.approved,
                })
            })
            .collect();
        encode(records)
    }

    pub(crate) fn handle_plan_apply(
        self: &Arc<Self>,
        params: Json,
        client: u64,
        attached: &mut Option<Attachment>,
        conn: Option<&SharedWriter>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let p: PlanApplyParams = decode(params)?;
        let plans = self.plans.lock().unwrap();
        let stored = plans.get(&p.plan_ref).ok_or_else(|| RpcError {
            code: -32012,
            message: "unknown plan_ref".into(),
            data: None,
        })?;
        if stored.session != session.id || stored.principal != attachment.principal {
            return Err(RpcError {
                code: -32010,
                message: "plan belongs to another principal/session".into(),
                data: None,
            });
        }
        if !stored.approved
            && self
                .policy
                .evaluate_plan(&attachment.principal, &stored.plan)
                != Verdict::Allow
        {
            return Err(RpcError {
                code: -32011,
                message: "plan approval pending".into(),
                data: None,
            });
        }
        let src = stored.src.clone();
        drop(plans);
        let response = self.dispatch(
            Request {
                jsonrpc: JSONRPC.into(),
                id: Json::Null,
                method: "exec".into(),
                params: serde_json::to_value(ExecParams {
                    src,
                    mode: "approved".into(),
                    position: "stmt".into(),
                    asynchronous: false,
                    timeout_ms: None,
                    elide: None,
                    plan_ref: Some(p.plan_ref.clone()),
                })
                .unwrap(),
            },
            client,
            attached,
            conn,
        );
        response.result.ok_or_else(|| {
            response
                .error
                .unwrap_or_else(|| internal("plan apply failed"))
        })
    }

    pub(crate) fn handle_cap_request(self: &Arc<Self>, params: Json) -> Result<Json, RpcError> {
        let p: CapRequestParams = decode(params)?;
        let Some(plan_ref) = p.plan_ref else {
            return Err(RpcError {
                code: -32602,
                message: "plan_ref is required".into(),
                data: None,
            });
        };
        let mut plans = self.plans.lock().unwrap();
        let stored = plans.get_mut(&plan_ref).ok_or_else(|| RpcError {
            code: -32012,
            message: "unknown plan_ref".into(),
            data: None,
        })?;
        if self.policy.evaluate_plan(&stored.principal, &stored.plan) == Verdict::Deny {
            return Err(RpcError {
                code: -32010,
                message: "policy denies requested effects".into(),
                data: None,
            });
        }
        // AGENT-SURFACE §5: if the caller scoped the request to a set
        // of effect kinds, the grant only covers those — a plan that
        // needs an effect the caller did not name stays pending, so an
        // approval can never silently widen past what was asked for.
        let requested: Vec<String> = p
            .effects
            .iter()
            .filter_map(|e| match e {
                Json::String(s) => Some(s.clone()),
                other => other.get("kind").and_then(Json::as_str).map(String::from),
            })
            .collect();
        if !requested.is_empty() {
            let requested: Vec<String> = requested.iter().map(|e| norm_effect(e)).collect();
            let missing: Vec<String> = stored
                .plan
                .effects
                .iter()
                .map(effect_kind)
                .filter(|k| !requested.contains(&norm_effect(k)))
                .collect();
            if !missing.is_empty() {
                return encode(json!({
                    "grant": "approval_pending",
                    "plan_ref": plan_ref,
                    "why": "requested effect scope does not cover the plan",
                    "uncovered_effects": missing,
                }));
            }
        }
        stored.approved = true;
        // Same honest enforcement truth `session.attach`'s `caps_enforced`
        // reports (docs/ROADMAP.md open-item #5) — not a hardcoded `false`.
        // An agent that just unstuck an `approval_pending` plan via
        // `cap.request` must learn whether the OS is actually going to
        // confine what it is about to run, the same way it would have
        // learned at attach time.
        let enforced = self.caps_enforced_for(&stored.principal);
        encode(
            json!({"grant":"approved","plan_ref":plan_ref,"enforced":enforced,"granted_effects":requested}),
        )
    }
}
