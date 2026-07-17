//! `dispatch` handlers for task lifecycle and plan approval: `task.list`,
//! `task.get`, `task.await`, `task.cancel`, `task.suspend`, `task.resume`,
//! `plan.apply`, `cap.request`. Split out of `lib.rs`'s dispatch match
//! Wire behavior is documented in `site/content/internals/kernel-protocol.md`.
use super::*;

/// Unwind-safe ownership of the transient `Granting` state. Ordinary errors,
/// handler panics, and dropped requests restore the exact pre-grant state. A
/// stale-state timeout in `PlanRegistry` is the deterministic backstop if the
/// process abandons a request without running destructors.
struct ApprovalGrantReservation {
    kernel: Arc<Kernel>,
    plan_ref: String,
    record: ApprovalRecord,
    _lease: Arc<()>,
    armed: bool,
}

impl ApprovalGrantReservation {
    fn new(kernel: Arc<Kernel>, plan_ref: String, record: ApprovalRecord, lease: Arc<()>) -> Self {
        Self {
            kernel,
            plan_ref,
            record,
            _lease: lease,
            armed: true,
        }
    }

    fn commit(&mut self, completed: ApprovalRecord) -> Result<(), RpcError> {
        self.kernel
            .plans
            .transaction(|plans| -> Result<(), RpcError> {
                let stored = plans.get_mut(&self.plan_ref).ok_or_else(|| {
                    internal("approval reservation disappeared after durable audit")
                })?;
                if !matches!(
                    &stored.authorization,
                    PlanAuthorization::Granting { record, .. } if record == &self.record
                ) {
                    return Err(internal("approval reservation changed after durable audit"));
                }
                stored.authorization = PlanAuthorization::Approved(completed);
                Ok(())
            })?;
        self.armed = false;
        Ok(())
    }

    fn rollback(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let plan_ref = self.plan_ref.clone();
        let record = self.record.clone();
        let kernel = self.kernel.clone();
        // Drop may run during another unwind. A poisoned plan mutex is already
        // quarantined by dispatch; never turn best-effort rollback into abort.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            kernel.plans.transaction(|plans| {
                if let Some(stored) = plans.get_mut(&plan_ref) {
                    let restore = match &stored.authorization {
                        PlanAuthorization::Granting {
                            record: current,
                            restore_policy_allowed,
                            ..
                        } if current == &record => Some(*restore_policy_allowed),
                        _ => None,
                    };
                    if let Some(restore_policy_allowed) = restore {
                        stored.authorization = if restore_policy_allowed {
                            PlanAuthorization::PolicyAllowed
                        } else {
                            PlanAuthorization::Pending
                        };
                    }
                }
            });
        }));
    }
}

impl Drop for ApprovalGrantReservation {
    fn drop(&mut self) {
        self.rollback();
    }
}

impl Kernel {
    pub(crate) fn handle_task_list(
        self: &Arc<Self>,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let owner = session.key.owner();
        self.reap_finished_tasks(&owner);
        let tasks = self.tasks.snapshot_owner(&owner);
        let records = tasks.iter().map(task_record).collect::<Vec<_>>();
        encode(records)
    }

    pub(crate) fn handle_task_get(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        self.reap_finished_tasks(&session.key.owner());
        let p: TaskParams = decode(params)?;
        let task = self.task(&p.task)?;
        if task.owner != session.key.owner() {
            return Err(RpcError {
                code: UNKNOWN_TASK,
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
        self.reap_finished_tasks(&session.key.owner());
        let p: TaskParams = decode(params)?;
        let task = self.task(&p.task)?;
        if task.owner != session.key.owner() {
            return Err(RpcError {
                code: UNKNOWN_TASK,
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
        self.reap_finished_tasks(&session.key.owner());
        let p: TaskParams = decode(params)?;
        let task = self.task(&p.task)?;
        if task.owner != session.key.owner() {
            return Err(RpcError {
                code: UNKNOWN_TASK,
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
        self.reap_finished_tasks(&session.key.owner());
        let p: TaskParams = decode(params)?;
        let task = self.task(&p.task)?;
        if task.owner != session.key.owner() {
            return Err(RpcError {
                code: UNKNOWN_TASK,
                message: "unknown task ref".into(),
                data: None,
            });
        }
        Err(RpcError {
            code: TASK_CONTROL_UNAVAILABLE,
            message: "task suspension is unavailable for evaluator-owned processes".into(),
            data: Some(json!({"task":p.task})),
        })
    }

    // site/content/internals/roadmap-and-priorities.md: added alongside the pre-existing
    // `task.suspend` above, honest in the same way — a kernel task is
    // a Rust thread recursively calling back into `dispatch`, not a
    // single tracked child process/group, so there is nothing here to
    // send `SIGCONT` to yet. Real suspend/resume for a task's spawned
    // children lands with the eval sibling's task-lifecycle methods
    // (`.suspend()`/`.resume()`); once a task's process handle is
    // reachable from here, this stub becomes the real thing without
    // changing the wire shape.
    pub(crate) fn handle_task_resume(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        self.reap_finished_tasks(&session.key.owner());
        let p: TaskParams = decode(params)?;
        let task = self.task(&p.task)?;
        if task.owner != session.key.owner() {
            return Err(RpcError {
                code: UNKNOWN_TASK,
                message: "unknown task ref".into(),
                data: None,
            });
        }
        Err(RpcError {
            code: TASK_CONTROL_UNAVAILABLE,
            message: "task resume is unavailable for evaluator-owned processes".into(),
            data: Some(json!({"task":p.task})),
        })
    }

    /// `plan.get` (site/content/internals/kernel-protocol.md, `shoal://plan/{ref}`): the stored plan a
    /// prior `exec {mode:"plan"}` / `shoal_plan` derived and keyed by its
    /// `plan:<full-hash>:<object-id>` ref — its canonical AST (re-parsed from the stored
    /// source), concrete effects, reversibility, and the *current* leash
    /// verdict, mirroring what `explain` returns plus the verdict fields
    /// `exec`'s plan mode reports. Session/principal-scoped like `plan.apply`
    /// (a plan is private to the principal that derived it), and an unknown or
    /// expired ref is a clear not-found (`UNKNOWN_PLAN`, `-32012`), never a
    /// silent empty plan.
    pub(crate) fn handle_plan_get(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let p: PlanApplyParams = decode(params)?;
        self.plans.transaction(|plans| {
            if plans.get(&p.plan_ref).is_some_and(plan_expired) {
                plans.remove(&p.plan_ref);
            }
            let stored = plans.get(&p.plan_ref).ok_or_else(|| RpcError {
                code: UNKNOWN_PLAN,
                message: "unknown or expired plan_ref".into(),
                data: Some(json!({ "plan_ref": p.plan_ref })),
            })?;
            if (stored.session != session.id || stored.principal != attachment.principal)
                && !attachment.can_approve
            {
                return Err(RpcError {
                    code: LEASH_DENIED,
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
            "approval_pending": stored.authorization.is_pending(),
            "approved": stored.authorization.is_approved(),
            // HR-D2: the auditable approval binding, when this plan was approved
            // — requester, approver, scope, when, and the consuming execution.
            "approval": approval_json(stored.authorization.approval()),
            "src": stored.src,
            }))
        })
    }

    /// `plan.list` (site/content/internals/kernel-protocol.md): the open plans this session/principal
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
        self.plans.transaction(|plans| {
            plans.retain(|_, stored| !plan_expired(stored));
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
                        "approval_pending": sp.authorization.is_pending(),
                        "approved": sp.authorization.is_approved(),
                    })
                })
                .collect();
            encode(records)
        })
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
        let src = self
            .plans
            .transaction(|plans| -> Result<String, RpcError> {
                if plans.get(&p.plan_ref).is_some_and(plan_expired) {
                    plans.remove(&p.plan_ref);
                }
                let stored = plans.get(&p.plan_ref).ok_or_else(|| RpcError {
                    code: UNKNOWN_PLAN,
                    message: "unknown plan_ref".into(),
                    data: None,
                })?;
                if stored.session != session.id || stored.principal != attachment.principal {
                    return Err(RpcError {
                        code: LEASH_DENIED,
                        message: "plan belongs to another principal/session".into(),
                        data: None,
                    });
                }
                match &stored.authorization {
                    PlanAuthorization::PolicyAllowed
                        if self
                            .policy
                            .evaluate_plan(&attachment.principal, &stored.plan)
                            == Verdict::Allow => {}
                    PlanAuthorization::Approved(_) => {}
                    PlanAuthorization::Pending => {
                        return Err(RpcError {
                            code: APPROVAL_REQUIRED,
                            message: "plan approval pending".into(),
                            data: None,
                        });
                    }
                    PlanAuthorization::Claimed(_) => {
                        return Err(RpcError {
                            code: LEASH_DENIED,
                            message: "approved plan is already being applied".into(),
                            data: None,
                        });
                    }
                    PlanAuthorization::Granting { .. } => {
                        return Err(RpcError {
                            code: LEASH_DENIED,
                            message: "approval grant is still being durably recorded".into(),
                            data: Some(json!({"plan_ref": p.plan_ref})),
                        });
                    }
                    PlanAuthorization::Consumed(record) => {
                        return Err(RpcError {
                            code: LEASH_DENIED,
                            message: "approval was already consumed".into(),
                            data: Some(json!({"consumed_by": record.consumed_by})),
                        });
                    }
                    PlanAuthorization::Denied | PlanAuthorization::PolicyAllowed => {
                        return Err(RpcError {
                            code: LEASH_DENIED,
                            message: "plan is denied by policy".into(),
                            data: None,
                        });
                    }
                }
                Ok(stored.src.clone())
            })?;
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

    pub(crate) fn handle_cap_request(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        // HR-D1: approving a plan is a state mutation — it must come from an
        // authenticated caller, never a bare unattached socket client (the
        // audit found this handler was dispatched with no attachment at all).
        // The attachment principal IS the approver, bound into the record below.
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let approver = attachment.principal.clone();
        let can_approve = attachment.can_approve;
        let p: CapRequestParams = decode(params)?;
        let Some(plan_ref) = p.plan_ref else {
            return Err(RpcError {
                code: INVALID_PARAMS,
                message: "plan_ref is required".into(),
                data: None,
            });
        };
        let requested: Vec<String> = p
            .effects
            .iter()
            .filter_map(|e| match e {
                Json::String(s) => Some(s.clone()),
                other => other.get("kind").and_then(Json::as_str).map(String::from),
            })
            .map(|e| norm_effect(&e))
            .collect();
        type PreparedApproval = (ApprovalRecord, Vec<String>, String, Arc<()>);
        type ApprovalPreparation = Result<PreparedApproval, Json>;

        // Phase one validates and reserves the transition under ONE plans
        // lock. Durable journal I/O deliberately happens after this
        // transaction; `Granting` excludes concurrent grants and applies.
        let grant_lease = Arc::new(());
        let prepared =
            self.plans
                .transaction(|plans| -> Result<ApprovalPreparation, RpcError> {
                    if plans.get(&plan_ref).is_some_and(plan_expired) {
                        plans.remove(&plan_ref);
                    }
                    let stored = plans.get_mut(&plan_ref).ok_or_else(|| RpcError {
                        code: UNKNOWN_PLAN,
                        message: "unknown plan_ref".into(),
                        data: None,
                    })?;

                    let requester = stored.principal.clone();
                    let self_ack =
                        approver == requester && self.allow_self_ack.load(Ordering::SeqCst);
                    if approver == requester && !self_ack {
                        return Err(RpcError {
                        code: LEASH_DENIED,
                        message:
                            "self-approval is not permitted: a plan's approver must differ from \
                              its requester (enable self-acknowledgement explicitly to override)"
                                .into(),
                        data: Some(json!({
                            "plan_ref": plan_ref,
                            "requester": requester,
                            "approver": approver,
                        })),
                    });
                    }
                    if approver != requester && !can_approve {
                        return Err(RpcError {
                            code: LEASH_DENIED,
                            message:
                                "approver is not authorized: use the embedded human trust root, \
                              supervisor profile, or plan.approve capability"
                                    .into(),
                            data: Some(json!({
                                "plan_ref": plan_ref,
                                "requester": requester,
                                "approver": approver,
                            })),
                        });
                    }
                    if self.policy.evaluate_plan(&stored.principal, &stored.plan) == Verdict::Deny {
                        return Err(RpcError {
                            code: LEASH_DENIED,
                            message: "policy denies requested effects".into(),
                            data: None,
                        });
                    }

                    let plan_effect_kinds = stored
                        .plan
                        .effects
                        .iter()
                        .map(effect_kind)
                        .collect::<Vec<_>>();
                    if !requested.is_empty() {
                        let missing: Vec<String> = plan_effect_kinds
                            .iter()
                            .filter(|k| !requested.contains(&norm_effect(k)))
                            .cloned()
                            .collect();
                        if !missing.is_empty() {
                            return Ok(Err(json!({
                                "grant": "approval_pending",
                                "plan_ref": plan_ref,
                                "why": "requested effect scope does not cover the plan",
                                "uncovered_effects": missing,
                            })));
                        }
                    }

                    match &stored.authorization {
                        PlanAuthorization::Pending => {}
                        PlanAuthorization::Granting { .. } => {
                            return Err(RpcError {
                                code: LEASH_DENIED,
                                message: "approval grant is already in progress".into(),
                                data: Some(json!({"plan_ref": plan_ref})),
                            });
                        }
                        PlanAuthorization::Approved(_) | PlanAuthorization::Claimed(_) => {
                            return Err(RpcError {
                                code: LEASH_DENIED,
                                message: "plan already has an approval".into(),
                                data: Some(json!({"plan_ref": plan_ref})),
                            });
                        }
                        PlanAuthorization::Consumed(record) => {
                            return Err(RpcError {
                                code: LEASH_DENIED,
                                message: "approval was already consumed; create a new plan".into(),
                                data: Some(json!({
                                    "plan_ref": plan_ref,
                                    "consumed_by": record.consumed_by,
                                })),
                            });
                        }
                        PlanAuthorization::Denied => {
                            return Err(RpcError {
                                code: LEASH_DENIED,
                                message: "policy denies requested effects".into(),
                                data: None,
                            });
                        }
                        // A caller may still request an explicit, auditable one-shot
                        // approval for a plan policy would allow directly. This keeps
                        // cap.request useful as an acknowledgement/audit operation.
                        PlanAuthorization::PolicyAllowed => {}
                    }

                    let restore_policy_allowed =
                        matches!(stored.authorization, PlanAuthorization::PolicyAllowed);
                    let record = ApprovalRecord {
                        requester: requester.clone(),
                        approver: approver.clone(),
                        plan_ref: stored.plan.plan_ref.clone(),
                        plan_hash: stored.plan_hash.clone(),
                        source_hash: stored.source_hash.clone(),
                        session: stored.session.clone(),
                        // Record the exact immutable plan scope, never a caller-supplied
                        // superset that could overstate what was actually approved.
                        scope: plan_effect_kinds.clone(),
                        approved_at_ns: now_ns(),
                        grant_audit_id: 0,
                        consumed_by: None,
                    };
                    stored.authorization = PlanAuthorization::Granting {
                        record: record.clone(),
                        restore_policy_allowed,
                        started_at: Instant::now(),
                        lease: Arc::downgrade(&grant_lease),
                    };
                    Ok(Ok((record, plan_effect_kinds, requester, grant_lease)))
                })?;
        let (mut record, plan_effect_kinds, requester, grant_lease) = match prepared {
            Ok(approved) => approved,
            Err(response) => return encode(response),
        };
        let mut reservation = ApprovalGrantReservation::new(
            self.clone(),
            plan_ref.clone(),
            record.clone(),
            grant_lease,
        );

        // Phase two performs the potentially blocking durable append with no
        // plan-registry guard held. The reservation guard restores the exact
        // source state on both ordinary errors and unwinding panics.
        let grant_audit_id =
            self.record_approval_audit(&record, &plan_effect_kinds, &record.session)?;

        // Phase three publishes the durable grant into the plan state. The
        // identity comparison makes this a compare-and-set, not a blind write.
        record.grant_audit_id = grant_audit_id;
        reservation.commit(record.clone())?;
        // Same honest enforcement truth `session.attach`'s `caps_enforced`
        // reports (see `site/content/internals/security-threat-model.md`) — not a hardcoded `false`.
        // An agent that just unstuck an `approval_pending` plan via
        // `cap.request` must learn whether the OS is actually going to
        // confine what it is about to run, the same way it would have
        // learned at attach time.
        let enforced = self.caps_enforced_for(&requester);
        encode(json!({
            "grant": "approved",
            "plan_ref": plan_ref,
            "enforced": enforced,
            "granted_effects": plan_effect_kinds,
            "requester": requester,
            "approver": approver,
        }))
    }
}
