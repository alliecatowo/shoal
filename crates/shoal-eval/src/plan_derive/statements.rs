//! Statement and block traversal for conservative plan derivation.

use super::*;
use crate::plan_effects::push_effect;

impl Evaluator {
    pub(super) fn plan_stmt(
        &mut self,
        stmt: &Stmt,
        functions: &Functions,
        aliases: &Aliases,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        match stmt {
            Stmt::Expr { expr, .. } => self.plan_expr(expr, functions, aliases, out, depth),
            Stmt::Let { init, .. } => self.plan_expr(init, functions, aliases, out, depth),
            Stmt::Assign { target, value, .. } => {
                self.plan_expr(value, functions, aliases, out, depth)?;
                if let Expr::Field { recv, name, .. } = target
                    && matches!(&**recv, Expr::Var { name: namespace, .. } if namespace == "env")
                {
                    push_effect(
                        out,
                        Effect::EnvWrite {
                            names: vec![name.clone()],
                        },
                    );
                    Ok(())
                } else {
                    self.plan_expr(target, functions, aliases, out, depth)
                }
            }
            Stmt::Use { path, .. } => {
                push_effect(
                    out,
                    Effect::FsRead {
                        paths: vec![self.plan_module_path(path)],
                    },
                );
                // Module top-level code is arbitrary and is not loaded during
                // planning, so approval remains mandatory.
                push_effect(out, Effect::Opaque);
                Ok(())
            }
            Stmt::Return {
                value: Some(expr), ..
            } => self.plan_expr(expr, functions, aliases, out, depth),
            Stmt::For { iter, body, .. } => {
                self.plan_expr(iter, functions, aliases, out, depth)?;
                self.plan_block(body, functions, aliases, out, depth)
            }
            Stmt::While { cond, body, .. } => {
                self.plan_expr(cond, functions, aliases, out, depth)?;
                self.plan_block(body, functions, aliases, out, depth)
            }
            Stmt::Fn { .. }
            | Stmt::Alias { .. }
            | Stmt::Return { value: None, .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => Ok(()),
        }
    }

    pub(super) fn plan_block(
        &mut self,
        block: &Block,
        functions: &Functions,
        aliases: &Aliases,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        for stmt in &block.stmts {
            self.plan_stmt(stmt, functions, aliases, out, depth)?;
        }
        Ok(())
    }
}
