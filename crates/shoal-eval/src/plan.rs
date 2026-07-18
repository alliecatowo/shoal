//! Leash plan derivation: a conservative, concrete-effect walk over the AST
//! that never spawns or mutates (site/content/internals/intercrate-protocol-contracts.md leash integration).
//!
//! Split across three files (the multi-file `impl Evaluator { .. }` pattern):
//! this file holds the user-facing `plan`/`apply`/`explain` builtins and the
//! plan-record rendering; [`crate::plan_derive`] holds the AST walk that
//! derives a [`Plan`]'s effect list; [`crate::plan_effects`] holds the
//! per-builtin/adapter effect computation the walk calls into.

use super::*;

impl Evaluator {
    /// `plan { … }` / `plan <cmd …>` (site/content/internals/roadmap-and-priorities.md): derive and render the effect
    /// plan without spawning or mutating. The derived program is stashed so a
    /// later `apply <ref>` can run it; the returned record carries its `id`.
    pub(crate) fn builtin_plan(&mut self, call: &CmdCall) -> VResult<Value> {
        let program = self.plan_target_program(call)?;
        let plan = self.plan_program(&program)?;
        let id = self.exec.plans.store(program)?;
        Ok(plan_record(&plan, Some(id)))
    }

    /// `apply <ref>` (site/content/internals/roadmap-and-priorities.md): run a previously-derived `plan { … }`. The ref
    /// is the record `plan` returned (its `id` field) or a bare plan id int.
    pub(crate) fn builtin_apply(&mut self, call: &CmdCall) -> VResult<Value> {
        let vs = self.collect_cmd_values(call)?;
        let id = match vs.first() {
            Some(Value::Int(n)) => *n,
            // A bare `apply 3` word arrives as a str; accept a numeric one.
            Some(Value::Str(s)) if s.parse::<i64>().is_ok() => s.parse().unwrap(),
            Some(Value::Record(r)) => match r.get("id") {
                Some(Value::Int(n)) => *n,
                _ => {
                    return Err(ErrorVal::arg_error(
                        "apply expects a plan reference (a `plan { … }` result or its id)",
                    ));
                }
            },
            _ => {
                return Err(ErrorVal::arg_error(
                    "apply expects a plan reference: `apply <plan>`",
                ));
            }
        };
        let program = self.exec.plans.program(id)?;
        self.eval_program(&program)
    }

    /// `explain(src)` (site/content/internals/roadmap-and-priorities.md): parse a source string and render what it
    /// would do — its effect plan — without running it.
    pub(crate) fn builtin_explain(&mut self, call: &CmdCall) -> VResult<Value> {
        let vs = self.collect_cmd_values(call)?;
        let src = match vs.first() {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Path(p)) => p.to_string_lossy().into_owned(),
            _ => return Err(ErrorVal::arg_error("explain expects a source string")),
        };
        let program = shoal_syntax::parse_with_ctx(&src, self.parse_context(false))
            .map_err(|e| ErrorVal::new("parse_error", e.to_string()))?;
        let plan = self.plan_program(&program)?;
        let mut r = match plan_record(&plan, None) {
            Value::Record(r) => r,
            _ => Record::new(),
        };
        r.insert("source".into(), Value::Str(src));
        Ok(Value::Record(r))
    }

    /// The program a `plan`/`apply` verb targets: a trailing `{ … }` block, or a
    /// bare `plan rm x` command reconstructed from the remaining words.
    fn plan_target_program(&self, call: &CmdCall) -> VResult<Program> {
        if let Some(block) = &call.trailing {
            return Ok(Program {
                stmts: block.stmts.clone(),
            });
        }
        let mut args = call.args.iter();
        let head = args.next().and_then(cmd_arg_word).ok_or_else(|| {
            ErrorVal::arg_error("plan expects a block `plan { … }` or a command `plan <cmd> …`")
        })?;
        let inner = CmdCall {
            head,
            forced: false,
            args: args.cloned().collect(),
            redirects: vec![],
            env_prefix: vec![],
            background: false,
            trailing: None,
            span: call.span,
        };
        Ok(Program {
            stmts: vec![Stmt::Expr {
                expr: Expr::Cmd {
                    call: Box::new(inner),
                    span: call.span,
                },
                span: call.span,
            }],
        })
    }
}

/// The literal word text of a command argument (for `plan <cmd> …` head/args).
fn cmd_arg_word(arg: &CmdArg) -> Option<String> {
    match arg {
        CmdArg::Word { text, .. } | CmdArg::Path { text, .. } => Some(text.clone()),
        _ => None,
    }
}

/// Render a derived [`Plan`] as a shoal record: `{id?, effects: [str], reversible,
/// spawns}`. Human-readable and machine-usable (the `id` feeds `apply`).
fn plan_record(plan: &Plan, id: Option<i64>) -> Value {
    let effects: Vec<Value> = plan
        .effects
        .iter()
        .map(|e| Value::Str(effect_str(e)))
        .collect();
    let spawns = plan
        .effects
        .iter()
        .any(|e| matches!(e, Effect::ProcSpawn { .. } | Effect::Opaque));
    let mut r = Record::new();
    if let Some(id) = id {
        r.insert("id".into(), Value::Int(id));
    }
    r.insert("effects".into(), Value::List(effects));
    r.insert(
        "reversible".into(),
        Value::Bool(matches!(plan.reversibility, Reversibility::Reversible)),
    );
    r.insert("spawns".into(), Value::Bool(spawns));
    Value::Record(r)
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A one-line human description of a single effect, for `plan`/`explain` output.
fn effect_str(e: &Effect) -> String {
    match e {
        Effect::FsRead { paths } => format!("read {}", join_paths(paths)),
        Effect::FsWrite { paths } => format!("write {}", join_paths(paths)),
        Effect::FsDelete { paths } => format!("delete {}", join_paths(paths)),
        Effect::ProcSpawn { argv0, .. } => format!("spawn {argv0}"),
        Effect::NetConnect { host, port } => format!("connect {host}:{port}"),
        Effect::NetListen { port } => format!("listen {port}"),
        Effect::EnvRead { names } => format!("env-read {}", names.join(", ")),
        Effect::EnvWrite { names } => format!("env-write {}", names.join(", ")),
        Effect::SecretUse { names } => format!("secret-use {}", names.join(", ")),
        Effect::SessionWrite => "session-write".to_string(),
        Effect::JournalRead => "journal-read".to_string(),
        Effect::Time => "read-clock".to_string(),
        Effect::Opaque => "opaque (effects unknown)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec_state::MAX_STORED_PLANS;

    #[test]
    fn applying_an_aged_out_plan_returns_stable_explicit_error() {
        let mut evaluator = Evaluator::new(std::env::temp_dir());
        let derive = shoal_syntax::parse("plan { null }").unwrap();
        for _ in 0..=MAX_STORED_PLANS {
            evaluator.eval_program(&derive).unwrap();
        }

        let apply = shoal_syntax::parse("apply 1").unwrap();
        let error = evaluator.eval_program(&apply).unwrap_err();
        assert_eq!(error.code, "plan_expired");
        assert!(
            error
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("fresh plan")
        );
    }
}
