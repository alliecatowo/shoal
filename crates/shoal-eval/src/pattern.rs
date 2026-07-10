//! `match` pattern matching, and the `let`/for-loop binding-pattern subset.

use super::*;

impl Evaluator {
    pub(crate) fn bind_pattern(&mut self, p: &Pattern, v: Value, mutable: bool) -> VResult<()> {
        match p {
            Pattern::Wildcard { .. } => Ok(()),
            Pattern::Bind { name, .. } => {
                self.env.declare(name.clone(), v, mutable);
                Ok(())
            }
            Pattern::Lit { expr, .. } => {
                if self.eval_expr(expr, Position::Value)? == v {
                    Ok(())
                } else {
                    Err(ErrorVal::new("custom", "pattern did not match"))
                }
            }
            Pattern::List { items, rest, .. } => {
                let Value::List(xs) = v else {
                    return Err(ErrorVal::new("type_error", "list pattern requires list"));
                };
                if xs.len() < items.len() {
                    return Err(ErrorVal::new("custom", "list pattern did not match"));
                }
                for (p, v) in items.iter().zip(xs.iter().cloned()) {
                    self.bind_pattern(p, v, mutable)?;
                }
                if let Some(n) = rest {
                    self.env.declare(
                        n.clone(),
                        Value::List(xs.into_iter().skip(items.len()).collect()),
                        mutable,
                    );
                }
                Ok(())
            }
            _ => Err(ErrorVal::new(
                "custom",
                "this pattern form is not supported for binding yet",
            )),
        }
    }
    pub(crate) fn pattern_matches(&mut self, p: &Pattern, v: &Value) -> VResult<bool> {
        match p {
            Pattern::Wildcard { .. } => Ok(true),
            Pattern::Bind { name, .. } => {
                self.env.declare(name.clone(), v.clone(), false);
                Ok(true)
            }
            Pattern::Lit { expr, .. } => Ok(self.eval_expr(expr, Position::Value)? == *v),
            Pattern::Range {
                start,
                end,
                inclusive,
                ..
            } => {
                let a = self.eval_expr(start, Position::Value)?;
                let b = self.eval_expr(end, Position::Value)?;
                Ok(
                    shoal_value::ops::binop(BinOp::Ge, v, &a)? == Value::Bool(true)
                        && shoal_value::ops::binop(
                            if *inclusive { BinOp::Le } else { BinOp::Lt },
                            v,
                            &b,
                        )? == Value::Bool(true),
                )
            }
            // `int n` / `str s` — runtime type test, bind on success (TDD §3.2).
            Pattern::Type { ty, name, .. } => {
                if v.type_name() == ty.name {
                    if let Some(n) = name {
                        self.env.declare(n.clone(), v.clone(), false);
                    }
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            // `{ field, field: subpat }` — open record match: scrutinee must be
            // a record containing every named field; extra fields are ignored.
            Pattern::Record { fields, .. } => {
                let Value::Record(map) = v else {
                    return Ok(false);
                };
                for f in fields {
                    let Some(fv) = map.get(&f.name) else {
                        return Ok(false);
                    };
                    let fv = fv.clone();
                    match &f.pattern {
                        Some(sub) => {
                            if !self.pattern_matches(sub, &fv)? {
                                return Ok(false);
                            }
                        }
                        None => self.env.declare(f.name.clone(), fv, false),
                    }
                }
                Ok(true)
            }
            // `[a, b, ...rest]` — shape match over a list.
            Pattern::List { items, rest, .. } => {
                let Value::List(xs) = v else {
                    return Ok(false);
                };
                if rest.is_some() {
                    if xs.len() < items.len() {
                        return Ok(false);
                    }
                } else if xs.len() != items.len() {
                    return Ok(false);
                }
                for (p, ev) in items.iter().zip(xs.iter()) {
                    let ev = ev.clone();
                    if !self.pattern_matches(p, &ev)? {
                        return Ok(false);
                    }
                }
                if let Some(r) = rest {
                    self.env
                        .declare(r.clone(), Value::List(xs[items.len()..].to_vec()), false);
                }
                Ok(true)
            }
        }
    }
    pub(crate) fn eval_match(&mut self, scrutinee: &Expr, arms: &[MatchArm]) -> VResult<Value> {
        let v = self.eval_expr(scrutinee, Position::Value)?;
        for arm in arms {
            let old = self.env.clone();
            self.env = old.child();
            let mut matched = false;
            for p in &arm.patterns {
                if self.pattern_matches(p, &v)? {
                    matched = true;
                    break;
                }
            }
            if matched
                && arm
                    .guard
                    .as_ref()
                    .map(|g| {
                        self.eval_expr(g, Position::Value)
                            .and_then(|x| x.as_condition())
                    })
                    .transpose()?
                    .unwrap_or(true)
            {
                let r = self.eval_expr(&arm.body, Position::Value);
                self.env = old;
                return r;
            }
            self.env = old;
        }
        Ok(Value::Null)
    }
}
