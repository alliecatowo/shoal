//! The AST walk that derives a conservative, concrete [`Plan`] (see
//! [`crate::plan`] for the split rationale). Never spawns or mutates;
//! per-builtin/adapter effect computation lives in [`crate::plan_effects`].

use super::*;

use crate::plan_effects::push_effect;

mod attribution;
mod commands;
mod inputs;
mod statements;
mod value_effects;

use attribution::{cmd_arg_str_literal, str_literal};

type Functions = std::collections::HashMap<String, Block>;
type Aliases = std::collections::HashMap<String, CmdCall>;

impl Evaluator {
    /// Derive a conservative, concrete plan without spawning or mutating.
    pub fn plan_program(&mut self, program: &Program) -> VResult<Plan> {
        let mut effects = Vec::new();
        let mut functions = Functions::new();
        let mut aliases = Aliases::new();
        for stmt in &program.stmts {
            if let Stmt::Fn { decl } = stmt {
                functions.insert(decl.name.clone(), decl.body.clone());
            }
            if let Stmt::Alias { name, target, .. } = stmt {
                aliases.insert(name.clone(), target.clone());
            }
        }
        for stmt in &program.stmts {
            self.plan_stmt(stmt, &functions, &aliases, &mut effects, 0)?;
        }
        let reversibility = if effects
            .iter()
            .any(|e| matches!(e, Effect::Opaque | Effect::FsDelete { .. }))
        {
            Reversibility::Unknown
        } else {
            Reversibility::Reversible
        };
        Ok(Plan::new(effects, reversibility, Estimates::default()))
    }

    pub(crate) fn plan_expr(
        &mut self,
        expr: &Expr,
        functions: &std::collections::HashMap<String, Block>,
        aliases: &std::collections::HashMap<String, CmdCall>,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        match expr {
            Expr::Cmd { call, .. } => self.plan_call(call, functions, aliases, out, depth),
            Expr::LangBlock { .. } => {
                push_effect(out, Effect::Opaque);
                Ok(())
            }
            Expr::Block { block, .. } | Expr::Spawn { body: block, .. } => {
                self.plan_block(block, functions, aliases, out, depth)
            }
            Expr::If {
                cond, then, r#else, ..
            } => {
                self.plan_expr(cond, functions, aliases, out, depth)?;
                self.plan_block(then, functions, aliases, out, depth)?;
                if let Some(other) = r#else {
                    self.plan_expr(other, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::Try { body, handler, .. } => {
                self.plan_block(body, functions, aliases, out, depth)?;
                self.plan_block(handler, functions, aliases, out, depth)
            }
            Expr::Catch { expr, handler, .. } => {
                self.plan_expr(expr, functions, aliases, out, depth)?;
                self.plan_expr(handler, functions, aliases, out, depth)
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.plan_expr(lhs, functions, aliases, out, depth)?;
                self.plan_expr(rhs, functions, aliases, out, depth)
            }
            Expr::Unary { expr, .. } => self.plan_expr(expr, functions, aliases, out, depth),
            Expr::Field { recv, name, .. } => {
                self.plan_field_effects(recv, name, out);
                self.plan_expr(recv, functions, aliases, out, depth)
            }
            Expr::Index { recv, index, .. } => {
                self.plan_expr(recv, functions, aliases, out, depth)?;
                self.plan_expr(index, functions, aliases, out, depth)
            }
            Expr::MethodCall {
                recv, name, args, ..
            } => {
                // `.feed(cmd)` bypasses builtin/adapter dispatch and spawns the
                // command via run_argv (A9): resolve the command operand as an
                // external spawn, exactly like the runtime — handled before the
                // generic traversal so it is not mis-resolved as a builtin.
                if name == "feed" && args.pos.len() == 1 && args.named.is_empty() {
                    return self.plan_feed(recv, &args.pos[0], functions, aliases, out, depth);
                }
                self.plan_method_effects(recv, name, args, out);
                self.plan_expr(recv, functions, aliases, out, depth)?;
                for e in &args.pos {
                    self.plan_expr(e, functions, aliases, out, depth)?;
                }
                for n in &args.named {
                    self.plan_expr(&n.value, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::FnCall { name, args, span } => {
                // Arguments always contribute their own effects — including the
                // bodies of lambda arguments to `parallel`/`retry`/`on`/`map`
                // (A8), via the `Expr::Lambda` arm.
                for e in &args.pos {
                    self.plan_expr(e, functions, aliases, out, depth)?;
                }
                for n in &args.named {
                    self.plan_expr(&n.value, functions, aliases, out, depth)?;
                }
                if self.plan_constructor_effects(name, args, out) {
                    return Ok(());
                }
                match name.as_str() {
                    // Effectful builtins invoked as functions (A8).
                    "run" => self.plan_run_target(args.pos.first().and_then(str_literal), out),
                    // `save(path, value)` writes the first argument.
                    "save" => {
                        let path = args.pos.first().and_then(|a| self.path_literal(a));
                        self.plan_save(path, out);
                    }
                    "open" => {
                        let path = args.pos.first().and_then(|a| self.path_literal(a));
                        self.plan_open(path, out);
                    }
                    // Clock reads.
                    "now" | "today" => push_effect(out, Effect::Time),
                    // Higher-order builtins: their closure bodies are already
                    // planned above via the Lambda arm; `assert` is pure.
                    "parallel" | "retry" | "on" | "assert" => {}
                    other => {
                        if let Some(body) = functions.get(other) {
                            // A function declared in this program: expand it.
                            self.plan_block(body, functions, aliases, out, depth + 1)?;
                        } else if self
                            .exec
                            .shell
                            .env
                            .get(other)
                            .is_some_and(|v| v.is_callable())
                        {
                            // A session-stored closure/function that cannot be
                            // statically expanded (A5): require approval, never
                            // report nothing.
                            push_effect(out, Effect::Opaque);
                        } else if self.is_command_name(other) {
                            // A bare name that resolves as a command runs as one
                            // (defect #5); plan it with command resolution.
                            self.plan_command_ref(other, *span, functions, aliases, out, depth)?;
                        } else {
                            // Not a known pure form, an expandable function, a
                            // session closure, or a command — cannot be proven
                            // effect-free (A5/A10).
                            push_effect(out, Effect::Opaque);
                        }
                    }
                }
                Ok(())
            }
            Expr::List { items, .. } => {
                for e in items {
                    self.plan_expr(e, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::Record { fields, .. } => {
                for f in fields {
                    self.plan_expr(&f.value, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::Range { start, end, .. } => {
                self.plan_expr(start, functions, aliases, out, depth)?;
                self.plan_expr(end, functions, aliases, out, depth)
            }
            Expr::With {
                cwd,
                env,
                reef,
                body,
                ..
            } => {
                if let Some(e) = cwd {
                    self.plan_expr(e, functions, aliases, out, depth)?
                }
                if let Some(e) = env {
                    self.plan_expr(e, functions, aliases, out, depth)?
                }
                if let Some(e) = reef {
                    self.plan_expr(e, functions, aliases, out, depth)?
                }
                self.plan_block(body, functions, aliases, out, depth)
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                self.plan_expr(scrutinee, functions, aliases, out, depth)?;
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.plan_expr(g, functions, aliases, out, depth)?
                    }
                    self.plan_expr(&arm.body, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            // A lambda's body effects surface wherever the lambda is used — the
            // higher-order builtins (`parallel`, `map`, …) will invoke it (A8).
            Expr::Lambda { body, .. } => self.plan_expr(body, functions, aliases, out, depth),
            Expr::StrInterp { parts, .. } => {
                for part in parts {
                    if let StrPart::Expr { expr } = part {
                        self.plan_expr(expr, functions, aliases, out, depth)?;
                    }
                }
                Ok(())
            }
            // Provably effect-free atoms: an empty effect set is correct here.
            // No wildcard arm — a new `Expr` variant must be classified here,
            // so an effectful form can never silently derive no effects (A10).
            Expr::Null { .. }
            | Expr::Bool { .. }
            | Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::Str { .. }
            | Expr::Size { .. }
            | Expr::Duration { .. }
            | Expr::Time { .. }
            | Expr::DateTime { .. }
            | Expr::Regex { .. } => Ok(()),
            Expr::Var { name, .. } => {
                if let Some(Value::Secret(secret)) = self.exec.shell.env.get(name) {
                    push_effect(
                        out,
                        Effect::SecretUse {
                            names: vec![secret.name],
                        },
                    );
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests;
