//! Plan derivation, identity, authorization, and effect normalization.

use super::*;

/// Derive a plan's real effects and give it a source-anchored `plan_ref`.
/// Distinct programs cannot collide merely because they have the same coarse
/// effect set; the identity binds both serialized AST and effects.
pub(super) fn derive_plan(evaluator: &mut Evaluator, ast: &Program, ast_json: &str) -> Plan {
    let mut plan = evaluator.plan_program(ast).unwrap_or_else(|_| {
        Plan::new(
            vec![Effect::Opaque],
            Reversibility::Unknown,
            Estimates::default(),
        )
    });
    plan.plan_ref = canonical_plan_ref(ast_json, &plan.effects);
    plan
}

pub(super) fn canonical_plan_ref(ast_json: &str, effects: &[Effect]) -> String {
    let effects_json = serde_json::to_string(effects).unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(ast_json.as_bytes());
    hasher.update(b"\0");
    hasher.update(effects_json.as_bytes());
    format!("plan:{}", hasher.finalize().to_hex())
}

pub(super) fn source_hash(src: &str) -> String {
    blake3::hash(src.as_bytes()).to_hex().to_string()
}

/// Full immutable approval binding. This deliberately excludes `plan_ref`
/// because that contains the per-kernel object id; all semantic inputs are
/// included explicitly and domain-separated.
pub(super) fn bound_plan_hash(
    src: &str,
    ast_json: &str,
    plan: &Plan,
    session: &str,
    requester: &str,
) -> String {
    let canonical = serde_json::to_vec(&(
        src,
        ast_json,
        &plan.effects,
        plan.reversibility,
        &plan.estimates,
        session,
        requester,
    ))
    .expect("plan binding is serializable");
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"shoal.kernel.plan-binding.v1\0");
    hasher.update(&canonical);
    hasher.finalize().to_hex().to_string()
}

/// `position: "value"` (site/content/internals/language-conformance-contract.md): evaluate the sole top-level command
/// expression without statement-position's raise-on-non-ok, binding `it` to
/// whatever comes back (including a failed outcome). Anything shaped other
/// than a single bare expression statement has no meaningful non-statement
/// reading (`let`/`fn`/`for`/… are already position-agnostic), so it falls
/// back to ordinary statement evaluation.
pub(super) fn eval_with_position(
    evaluator: &mut Evaluator,
    ast: &Program,
    position: &str,
) -> shoal_value::VResult<Value> {
    if position == "value"
        && let Some((last, init)) = ast.stmts.split_last()
    {
        // Run every statement but the last with ordinary statement semantics,
        // sharing the evaluator's env so bindings carry into the final expr.
        if !init.is_empty() {
            evaluator.eval_program(&Program {
                stmts: init.to_vec(),
            })?;
        }
        // site/content/internals/language-conformance-contract.md: the *final* expression is the value; evaluate it in value
        // position so a failed outcome is captured (bound to `it`), not raised.
        if let Stmt::Expr { expr, .. } = last {
            let value = evaluator.eval_expr(expr, Position::Value)?;
            evaluator.set_it(value.clone());
            return Ok(value);
        }
        // A final `let`/`fn`/`for`/… has no distinct value reading; run it as
        // a statement and return whatever it produces.
        return evaluator.eval_program(&Program {
            stmts: vec![last.clone()],
        });
    }
    evaluator.eval_program(ast)
}
pub(super) fn verdict_name(v: Verdict) -> &'static str {
    match v {
        Verdict::Allow => "allow",
        Verdict::Deny => "deny",
        Verdict::ApprovalRequired => "approval_required",
    }
}

/// Derive plan reversibility from its concrete effects (see
/// `site/content/internals/kernel-protocol.md`): irreversible for opaque work or network effects; reversible when
/// every effect is reversible/journaled (pure reads/writes, env, session,
/// time — AND recoverable filesystem deletes, see below). This is computed here rather
/// than trusting the leash's coarser `Reversibility` so the wire answer is
/// derived from the effect set the agent actually sees.
///
/// **`Effect::FsDelete` and the trash-vs-permanent distinction:** the only two
/// builtins that ever emit `FsDelete` are `rm` and `mv` (`shoal-eval`'s
/// `plan_effects.rs`); `sh{}`/any external command emits `Effect::Opaque`
/// instead and NEVER `FsDelete` — the two are structurally disjoint by
/// construction of the planner, so an `FsDelete` effect can never originate
/// from an opaque `sh { rm -rf }` (that stays caught by the `Opaque` arm
/// below, unconditionally). `FsDelete { permanent: false }` is reversible:
/// shoal's default `rm` moves files into a journaled trash
/// (`apply` fully recovers them; see `shoal-eval`'s `fs_undo_post`/
/// `record_trash_inverses` and `shoal-journal`'s `UndoInverse::TrashMove`),
/// and `mv`'s source-clearing "delete" is likewise undoable
/// (`UndoInverse::MoveBack`/`RestoreBytes`). `rm --permanent` instead derives
/// `FsDelete { permanent: true }`, preserving the same `fs.delete` policy
/// capability while making the irreversible execution mode explicit.
pub(super) fn reversibility_from_effects(effects: &[Effect]) -> &'static str {
    let irreversible = effects.iter().any(|effect| {
        effect.is_permanent_delete()
            || matches!(
                effect,
                Effect::Opaque | Effect::NetConnect { .. } | Effect::NetListen { .. }
            )
    });
    if irreversible {
        "irreversible"
    } else {
        "reversible"
    }
}

/// The `kind` tag an effect serializes with (`{"kind":"fs.write",…}`), used to
/// scope a `cap.request` grant to a set of effect kinds (site/content/internals/kernel-protocol.md).
pub(super) fn effect_kind(effect: &Effect) -> String {
    serde_json::to_value(effect)
        .ok()
        .and_then(|v| v.get("kind").and_then(Json::as_str).map(String::from))
        .unwrap_or_default()
}

/// Normalize an effect kind so the agent-facing dotted convention (`fs.delete`,
/// per site/content/internals/kernel-protocol.md) matches the snake_case form the effect actually
/// serializes to (`fs_delete`).
pub(super) fn norm_effect(kind: &str) -> String {
    kind.replace('.', "_")
}
