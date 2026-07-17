//! Authorized foreground evaluation, audit persistence, and response shaping.

use super::*;

struct ExecCompletion<'a> {
    params: &'a ExecParams,
    attachment: &'a Attachment,
    session: &'a Session,
    actor: &'a str,
    entry_id: i64,
    started: Instant,
    evaluator_started_ns: i64,
}

impl Kernel {
    pub(super) fn handle_exec_run(
        &self,
        params: ExecParams,
        attachment: &Attachment,
        session: &Arc<Session>,
        actor: String,
        interactive: bool,
    ) -> Result<Json, RpcError> {
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
        let claimed_approval =
            self.claim_exec_approval(&params, session, &actor, &ast_json, &run_plan)?;
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
        // Everything after an approval claim and before durable append is one
        // rollback region. Future fallible audit shaping must stay inside it.
        let append_result = (|| -> Result<i64, RpcError> {
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
            let journal = self
                .journal
                .lock()
                .map_err(|_| poisoned_subsystem("journal"))?;
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
        })();
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
                {
                    let journal = self
                        .journal
                        .lock()
                        .map_err(|_| poisoned_subsystem("journal"))?;
                    if let Some(stderr) = &e.stderr {
                        journal
                            .record_output(entry_id, "stderr", stderr.as_bytes())
                            .map_err(internal)?;
                    }
                    journal
                        .finish(entry_id, e.status, false, elapsed_ns(started))
                        .map_err(internal)?;
                }
                self.events.publish_journal(
                    &session.key.owner(),
                    entry_id,
                    journal_event(entry_id, &params.src, false, &actor),
                );
                // site/content/internals/kernel-protocol.md: even a raised error is
                // addressable — store it as an out[n] transcript value
                // so the agent can `shoal_get` the structured error
                // (code/msg/span/hint) instead of parsing message text.
                let value_ref = Ref::new("out", session.next_value.fetch_add(1, Ordering::Relaxed));
                let stored = session.insert_transcript_checked(
                    value_ref.clone(),
                    Value::Error(std::sync::Arc::new(e.clone())),
                );
                let uri = short_ref_to_uri(&value_ref, None);
                let mut data = json!({
                    "code": e.code, "span": e.span, "hint": e.hint,
                    "status": e.status, "stderr": e.stderr,
                });
                if let Err(store_error) = stored {
                    data["ref_unavailable"] = Json::Bool(true);
                    data["transcript_error"] = json!({
                        "code": store_error.code,
                        "message": store_error.message,
                    });
                } else {
                    data["ref"] = serde_json::to_value(value_ref).map_err(internal)?;
                    data["uri"] = Json::String(uri);
                }
                return Err(RpcError {
                    code: RAISED,
                    message: e.msg,
                    data: Some(data),
                });
            }
        };
        self.complete_exec_value(
            &mut evaluator,
            value,
            ExecCompletion {
                params: &params,
                attachment,
                session,
                actor: &actor,
                entry_id,
                started,
                evaluator_started_ns,
            },
        )
    }

    fn complete_exec_value(
        &self,
        evaluator: &mut Evaluator,
        value: Value,
        completion: ExecCompletion<'_>,
    ) -> Result<Json, RpcError> {
        let ExecCompletion {
            params,
            attachment,
            session,
            actor,
            entry_id,
            started,
            evaluator_started_ns,
        } = completion;
        let exit_code = evaluator.take_exit();
        let evaluator_entry_id = self
            .journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))?
            .query(&JournalQuery {
                since_ts_ns: Some(evaluator_started_ns),
                session: Some(session.id.clone()),
                principal: Some(actor.to_owned()),
                ok: Some(true),
                limit: 1,
                ..Default::default()
            })
            .ok()
            .and_then(|rows| rows.first().map(|row| row.id));
        // Keep the evaluator-visible REPL transcript (`it` and `out`) in
        // lockstep with the kernel's addressable Session transcript. Failed
        // evaluations intentionally do not reach this point, matching the
        // standalone REPL's successful-value-only contract.
        // Reserve and hold the side-map transaction before advancing `it/out`:
        // after this point insertion and retention are infallible HashMap
        // operations, so a transcript-lock/reservation failure cannot leave
        // evaluator state ahead of the addressable Session transcript.
        let mut transcript = session.lock_transcript()?;
        transcript.try_reserve(1).map_err(|error| RpcError {
            code: INTERNAL_ERROR,
            message: format!("cannot reserve session transcript entry: {error}"),
            data: Some(json!({"resource": "session_transcript"})),
        })?;
        if let Err(e) = evaluator.record_transcript(&value) {
            drop(transcript);
            {
                let journal = self
                    .journal
                    .lock()
                    .map_err(|_| poisoned_subsystem("journal"))?;
                journal
                    .finish(entry_id, e.status, false, elapsed_ns(started))
                    .map_err(internal)?;
            }
            self.events.publish_journal(
                &session.key.owner(),
                entry_id,
                journal_event(entry_id, &params.src, false, actor),
            );
            return Err(RpcError {
                code: RAISED,
                message: e.msg,
                data: Some(json!({
                    "code": e.code, "span": e.span, "hint": e.hint,
                    "status": e.status, "stderr": e.stderr
                })),
            });
        }
        let value_ref = Ref::new("out", session.next_value.fetch_add(1, Ordering::Relaxed));
        Session::insert_transcript_retained(&mut transcript, value_ref.clone(), value.clone());
        drop(transcript);
        session.push_out_entry(evaluator_entry_id);
        let render = shoal_value::render::render_block(&value, 80);
        // Built once, up front: this SAME payload is both persisted durably
        // (so the `session.transcript` channel can replay it after it ages
        // out of the ring (see `site/content/internals/kernel-protocol.md`) and carried
        // by the live event below. Reconstruction re-wraps the durable copy
        // verbatim rather than re-deriving it from other journal columns.
        let transcript_payload = transcript_event(&value_ref, &value);
        let transcript_ts = now_ns();
        let value_output = serde_json::to_vec(&wire_value(&value)).map_err(internal)?;
        let transcript_output = serde_json::to_string(&transcript_payload).map_err(internal)?;
        {
            let journal = self
                .journal
                .lock()
                .map_err(|_| poisoned_subsystem("journal"))?;
            let persisted = (|| -> Result<(), RpcError> {
                journal
                    .record_output(entry_id, "value", &value_output)
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
                    .record_transcript_event(entry_id, transcript_ts, &transcript_output)
                    .map_err(internal)?;
                // Success is the commit marker and must be written only after
                // every output required to replay the response is durable.
                journal
                    .finish(entry_id, Some(0), true, elapsed_ns(started))
                    .map_err(internal)
            })();
            if let Err(error) = persisted {
                let _ = journal.finish(entry_id, None, false, elapsed_ns(started));
                return Err(error);
            }
        }
        self.events.publish_journal(
            &session.key.owner(),
            entry_id,
            journal_event(entry_id, &params.src, true, actor),
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

    fn claim_exec_approval(
        &self,
        params: &ExecParams,
        session: &Session,
        actor: &str,
        ast_json: &str,
        run_plan: &Plan,
    ) -> Result<Option<ApprovalRecord>, RpcError> {
        let claimed_approval = if params.mode == "approved" {
            let Some(plan_ref) = params.plan_ref.as_ref() else {
                return Err(RpcError {
                    code: LEASH_DENIED,
                    message: "approved execution requires plan_ref".into(),
                    data: None,
                });
            };
            let actual_hash = bound_plan_hash(&params.src, ast_json, run_plan, &session.id, actor);
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
                            if self.policy.evaluate_plan(actor, &stored.plan) == Verdict::Allow =>
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
        Ok(claimed_approval)
    }
}
