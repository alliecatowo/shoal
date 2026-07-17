//! Unary/binary operator evaluation, including the `&&`/`||` short-circuit
//! chain (see [`crate::expr`] for the split rationale).

use super::*;

impl Evaluator {
    pub(crate) fn eval_unary(&mut self, op: &UnOp, expr: &Expr) -> VResult<Value> {
        let v = self.eval_expr(expr, Position::Value)?;
        match (op, v) {
            (UnOp::Not, v) => Ok(Value::Bool(!v.as_condition()?)),
            (UnOp::Neg, Value::Int(i)) => i
                .checked_neg()
                .map(Value::Int)
                .ok_or_else(|| ErrorVal::new("overflow", "integer negation overflow")),
            (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
            (UnOp::Neg, Value::Duration(n)) => n
                .checked_neg()
                .map(Value::Duration)
                .ok_or_else(|| ErrorVal::new("overflow", "duration negation overflow")),
            (_, v) => Err(ErrorVal::new(
                "type_error",
                format!("cannot apply unary operator to {}", v.type_name()),
            )),
        }
    }

    pub(crate) fn eval_binary(&mut self, expr: &Expr, position: Position) -> VResult<Value> {
        let Expr::Binary { op, lhs, rhs, .. } = expr else {
            unreachable!("eval_binary called on a non-Binary expr")
        };
        match op {
            BinOp::And | BinOp::Or => self.eval_chain(expr, position == Position::Statement),
            BinOp::Coalesce => {
                let l = self.eval_expr(lhs, Position::Value)?;
                if l == Value::Null {
                    self.eval_expr(rhs, Position::Value)
                } else {
                    Ok(l)
                }
            }
            _ => {
                let l = self.eval_expr(lhs, Position::Value)?;
                let r = self.eval_expr(rhs, Position::Value)?;
                shoal_value::ops::binop(*op, &l, &r)
            }
        }
    }

    /// Evaluate an `&&`/`||` chain (outcome unification, P1d). Per the normative
    /// corpus (`spec/cases/outcome.toml`, site/content/internals/language-conformance-contract.md) the operators are
    /// NOT bool-narrowing: they return the short-circuiting operand **verbatim**
    /// (whichever side's `as_condition()` decided the result), so a chain of
    /// outcome commands stays chainable — `(echo a && echo b).status` still
    /// works. Operands run in *value* position so a failed command surfaces as
    /// an outcome the chain short-circuits on rather than raising (letting
    /// `sh{exit 1} || echo x` recover). When `emit` (statement/discard context)
    /// every executed command operand's output is routed to the sink EXCEPT the
    /// returned one (the caller renders that once), so `echo a && echo b` prints
    /// both and an arbitrarily long chain prints every stage.
    pub(crate) fn eval_chain(&mut self, e: &Expr, emit: bool) -> VResult<Value> {
        let Expr::Binary {
            op: op @ (BinOp::And | BinOp::Or),
            lhs,
            rhs,
            span,
        } = e
        else {
            // Leaf: an ordinary sub-expression (a command, a bool, …).
            return self.eval_expr(e, Position::Value);
        };
        let l = self.eval_chain(lhs, emit)?;
        let ok = l.as_condition().map_err(|err| err.or_span(*span))?;
        let short = match op {
            BinOp::And => !ok,
            BinOp::Or => ok,
            _ => unreachable!(),
        };
        if short {
            // The short-circuiting operand decides — returned verbatim, not sunk
            // here (the caller renders it once).
            Ok(l)
        } else {
            // `l` is no longer the returned operand: print it if it was a
            // command outcome, then the rhs decides.
            if emit && crate::helpers::is_command_expr(lhs) {
                self.sink_value(&l);
            }
            self.eval_chain(rhs, emit)
        }
    }
}
