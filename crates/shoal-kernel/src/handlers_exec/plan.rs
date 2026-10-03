//! Plan derivation, bounded storage, and approval notification for `exec`.

use super::*;

impl Kernel {
    pub(super) fn handle_exec_plan(
        &self,
        params: ExecParams,
        session: &Arc<Session>,
        actor: &str,
        interactive: bool,
    ) -> Result<Json, RpcError> {
        let mut evaluator = session.lock_evaluator()?;
        let mut ast =
            shoal_syntax::parse_with_ctx(&params.src, evaluator.parse_context(interactive))
                .map_err(|error| RpcError {
                    code: PARSE_ERROR,
                    message: error.msg,
                    data: Some(json!({"span":error.span,"hint":error.hint})),
                })?;
        session.rewrite_out_undo(&mut ast);
        let ast_json = serde_json::to_string(&ast).map_err(internal)?;
        let plan = derive_plan(&mut evaluator, &ast, &ast_json);
        drop(evaluator);

        let source_hash = source_hash(&params.src);
        let plan_hash = bound_plan_hash(&params.src, &ast_json, &plan, &session.id, actor);
        let plan_ref = self.allocate_plan_ref(&plan_hash);
        let mut plan = plan;
        plan.plan_ref.clone_from(&plan_ref);
        let verdict = self.policy.evaluate_plan(actor, &plan);
        let effects = plan
            .effects
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;
        let result = PlanResult {
            plan_ref: plan.plan_ref.clone(),
            effects,
            reversibility: reversibility_from_effects(&plan.effects).into(),
            verdict: verdict_name(verdict).into(),
            approval_pending: verdict == Verdict::ApprovalRequired,
            enforcement: self.enforcement_preview(actor),
        };
        self.plans.transaction(|plans| -> Result<(), RpcError> {
            plans.retain(|_, stored| !plan_expired(stored));
            let owner_plans = plans
                .values()
                .filter(|stored| stored.session == session.id && stored.principal == actor)
                .collect::<Vec<_>>();
            let owner_source_bytes = owner_plans
                .iter()
                .map(|stored| stored.src.len())
                .sum::<usize>();
            if owner_plans.len() >= MAX_STORED_PLANS_PER_OWNER
                || owner_source_bytes.saturating_add(params.src.len())
                    > MAX_PLAN_SOURCE_BYTES_PER_OWNER
            {
                return Err(RpcError {
                    code: QUOTA_EXCEEDED,
                    message: "stored plan quota reached".into(),
                    data: Some(json!({
                        "limit": "stored_plans_per_owner",
                        "max_count": MAX_STORED_PLANS_PER_OWNER,
                        "max_source_bytes": MAX_PLAN_SOURCE_BYTES_PER_OWNER,
                    })),
                });
            }
            plans.insert(
                plan.plan_ref.clone(),
                StoredPlan {
                    src: params.src,
                    session: session.id.clone(),
                    principal: actor.into(),
                    plan_hash,
                    source_hash,
                    plan,
                    authorization: match verdict {
                        Verdict::Allow => PlanAuthorization::PolicyAllowed,
                        Verdict::ApprovalRequired => PlanAuthorization::Pending,
                        Verdict::Deny => PlanAuthorization::Denied,
                    },
                    created_at: Instant::now(),
                },
            );
            Ok(())
        })??;
        if verdict == Verdict::ApprovalRequired {
            // A plan stuck at `approval_pending` is the point at which another
            // principal needs a notification rather than another polling loop.
            self.events.publish(
                &session.key.owner(),
                "approval",
                approval_event(&result.plan_ref, &result.effects, actor),
            );
        }
        encode(result)
    }
}
