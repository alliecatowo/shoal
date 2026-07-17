//! Program/statement/block control flow: the top-level statement loop, `let`/
//! `fn`/`assign`/`for`/`while` statement forms, and block evaluation
//! (including the sink/discard-context bookkeeping the double-echo fix needs).

use super::*;

impl Evaluator {
    pub fn eval_program(&mut self, program: &Program) -> VResult<Value> {
        let mut last = Value::Null;
        let n = program.stmts.len();
        for (i, stmt) in program.stmts.iter().enumerate() {
            let is_last = i + 1 == n;
            // site/content/internals/language-conformance-contract.md: when a journal is installed, each top-level statement
            // becomes an entry (append → finish). A `None` journal makes this a
            // no-op, so scripts/-c/conformance are unaffected.
            let journaled = self.journal_begin_stmt(stmt);
            let result = self.eval_stmt(stmt, true);
            self.journal_finish_stmt(journaled, &result);
            match result? {
                Flow::Value(v) => {
                    self.exec.it = v.clone();
                    if is_last {
                        last = v;
                    } else if self.echo_intermediate(stmt) {
                        // Non-final statement values pass through to the sink
                        // (defect #1a); the final value is returned to the host.
                        // Under `render.echo = quiet`/`commands` (see
                        // `site/content/internals/configuration-reference.md`) only bare-command intermediates echo — a pure
                        // expression (`1+1`, `let x=…`) at intermediate position
                        // stays silent, so a non-interactive script no longer
                        // prints every step.
                        self.sink_value(&v);
                    }
                }
                Flow::Return(_) => {
                    return Err(
                        ErrorVal::new("custom", "return outside function").with_span(stmt.span())
                    );
                }
                Flow::Break | Flow::Continue => {
                    return Err(
                        ErrorVal::new("custom", "loop control outside loop").with_span(stmt.span())
                    );
                }
            }
            // `exit`/`quit` halts the remaining statements immediately; the host
            // reads `take_exit` and ends with the code (defect: no exit).
            if self.exec.pending_exit.is_some() {
                break;
            }
        }
        Ok(last)
    }

    /// Whether a non-final top-level statement's value routes to the statement
    /// sink, per the active [`EchoMode`] (`render.echo`, site/content/internals/configuration-reference.md).
    /// `All` (the default) echoes every intermediate — the REPL/legacy
    /// behavior; `Quiet`/`Commands` echo only bare-command intermediates so a
    /// script's intermediate pure expressions (`1+1`, `let x=…`) stay silent.
    fn echo_intermediate(&self, stmt: &Stmt) -> bool {
        match self.session.echo_mode {
            EchoMode::All => true,
            EchoMode::Quiet | EchoMode::Commands => crate::is_bare_command_stmt(stmt),
        }
    }

    pub(crate) fn eval_stmt(&mut self, stmt: &Stmt, top: bool) -> VResult<Flow> {
        match stmt {
            Stmt::Let {
                pattern,
                init,
                mutable,
                ..
            } => {
                let value = self.eval_expr(init, Position::Value)?;
                self.bind_pattern(pattern, value, *mutable)?;
                Ok(Flow::Value(Value::Null))
            }
            Stmt::Fn { decl } => {
                let closure = Value::Closure(Arc::new(ClosureVal {
                    name: Some(decl.name.clone()),
                    params: decl.params.clone(),
                    rest: decl.rest.clone(),
                    ret: decl.ret.clone(),
                    body: Expr::Block {
                        block: decl.body.clone(),
                        span: decl.body.span,
                    },
                    env: self.exec.env.clone(),
                    doc: decl.doc.clone(),
                }));
                self.exec.env.declare(decl.name.clone(), closure, false);
                Ok(Flow::Value(Value::Null))
            }
            Stmt::Alias { name, target, .. } => {
                self.exec
                    .env
                    .declare(name.clone(), Value::CmdRef(Arc::new(target.clone())), false);
                Ok(Flow::Value(Value::Null))
            }
            Stmt::Assign {
                target,
                op,
                value,
                span,
            } => {
                let rhs = self.eval_expr(value, Position::Value)?;
                // `env.NAME = v` — session environment write (defect #11, site/content/internals/language-conformance-contract.md).
                if let Expr::Field { recv, name, .. } = target
                    && matches!(&**recv, Expr::Var { name, .. } if name == "env")
                {
                    if *op != AssignOp::Set {
                        return Err(ErrorVal::new(
                            "type_error",
                            "compound assignment is not allowed on env.NAME",
                        )
                        .with_span(*span));
                    }
                    if self.exec.in_fn_body > 0 {
                        return Err(ErrorVal::new(
                            "custom",
                            "env writes are only allowed at session top level; use `with env:` inside a fn body",
                        )
                        .with_span(*span));
                    }
                    let val = self.argv_value(rhs.clone()).map_err(|e| e.or_span(*span))?;
                    self.exec
                        .process_env
                        .retain(|(k, _)| k != &OsString::from(name.clone()));
                    self.exec
                        .process_env
                        .push((OsString::from(name.clone()), val));
                    return Ok(Flow::Value(rhs));
                }
                let Expr::Var { name, .. } = target else {
                    return Err(ErrorVal::new(
                        "type_error",
                        "assignment target must be a variable in v0.1",
                    )
                    .with_span(*span));
                };
                let assigned = if *op == AssignOp::Set {
                    rhs
                } else {
                    let lhs = self.exec.env.get(name).ok_or_else(|| {
                        ErrorVal::new("undefined_var", format!("undefined variable `{name}`"))
                    })?;
                    let bop = match op {
                        AssignOp::Add => BinOp::Add,
                        AssignOp::Sub => BinOp::Sub,
                        AssignOp::Mul => BinOp::Mul,
                        AssignOp::Div => BinOp::Div,
                        AssignOp::Set => unreachable!(),
                    };
                    shoal_value::ops::binop(bop, &lhs, &rhs)?
                };
                self.exec.env.assign(name, assigned.clone()).map_err(|e| {
                    ErrorVal::new("type_error", format!("cannot assign `{name}`: {e:?}"))
                })?;
                Ok(Flow::Value(assigned))
            }
            Stmt::Expr { expr, .. } => {
                let position = if top {
                    Position::Statement
                } else {
                    Position::Value
                };
                self.eval_expr_flow(expr, position)
            }
            Stmt::Return { value, .. } => Ok(Flow::Return(match value {
                Some(v) => self.eval_expr(v, Position::Value)?,
                None => Value::Null,
            })),
            Stmt::Break { .. } => Ok(Flow::Break),
            Stmt::Continue { .. } => Ok(Flow::Continue),
            Stmt::For {
                pattern,
                iter,
                body,
                ..
            } => {
                let iter_value = self.eval_expr(iter, Position::Value)?;
                let vals = self.values_from(iter_value)?;
                for value in vals {
                    let old = self.exec.env.clone();
                    self.exec.env = old.child();
                    self.bind_pattern(pattern, value, false)?;
                    let flow = self.eval_block(body, true);
                    self.exec.env = old;
                    match flow? {
                        Flow::Value(_) => {}
                        Flow::Continue => continue,
                        Flow::Break => break,
                        r @ Flow::Return(_) => return Ok(r),
                    }
                }
                // A loop is a statement, not an expression — it yields no value
                // (so a trailing bare command in the body is not re-rendered as
                // the loop's result). Its work is its side effects.
                Ok(Flow::Value(Value::Null))
            }
            Stmt::While { cond, body, .. } => {
                while self.eval_expr(cond, Position::Value)?.as_condition()? {
                    match self.eval_block(body, true)? {
                        Flow::Value(_) => {}
                        Flow::Continue => {}
                        Flow::Break => break,
                        r @ Flow::Return(_) => return Ok(r),
                    }
                }
                Ok(Flow::Value(Value::Null))
            }
            Stmt::Use { path, span } => {
                self.eval_use(path, *span)?;
                Ok(Flow::Value(Value::Null))
            }
        }
    }

    /// Evaluate an expression appearing in statement position while letting
    /// `break`/`continue`/`return` inside an `if`/block body propagate to the
    /// enclosing loop rather than being flattened into a "loop control outside
    /// loop" error (the `while … { if … { break } }` case). Non-control-flow
    /// expressions fall back to ordinary value evaluation.
    pub(crate) fn eval_expr_flow(&mut self, expr: &Expr, position: Position) -> VResult<Flow> {
        match expr {
            Expr::If {
                cond,
                then,
                r#else,
                span,
            } => {
                let taken = self
                    .eval_expr(cond, Position::Value)?
                    .as_condition()
                    .map_err(|e| e.or_span(*span))?;
                if taken {
                    self.eval_block(then, false)
                } else if let Some(e) = r#else {
                    self.eval_expr_flow(e, position)
                } else {
                    Ok(Flow::Value(Value::Null))
                }
            }
            Expr::Block { block, .. } => self.eval_block(block, false),
            _ => Ok(Flow::Value(self.eval_expr(expr, position)?)),
        }
    }

    /// Evaluate a block. `sink_tail` says whether the caller will DISCARD the
    /// block's value (loop bodies) versus CONSUME it (fn body, `if`/block used
    /// as a value, or a top-level statement whose value `eval_program` itself
    /// sinks). To prevent double echo, the trailing bare-command statement is
    /// the block VALUE and must NOT also be sunk here — only when the caller
    /// discards it (`sink_tail`) does its output route to the sink; otherwise
    /// the caller renders/sinks it exactly once. Non-final bare commands always
    /// print (they are intermediate, discard-context regardless).
    pub(crate) fn eval_block(&mut self, block: &Block, sink_tail: bool) -> VResult<Flow> {
        let old = self.exec.env.clone();
        self.exec.env = old.child();
        let mut last = Flow::Value(Value::Null);
        let n = block.stmts.len();
        for (i, stmt) in block.stmts.iter().enumerate() {
            let is_tail = i + 1 == n;
            // A statement is in discard context when it is not the block value,
            // or when the caller discards the block value.
            let discard = !is_tail || sink_tail;
            if let Stmt::Expr { expr, .. } = stmt
                && crate::helpers::is_command_expr(expr)
            {
                // Discard-context commands run in statement position (failures
                // raise) and print; the value-context tail runs in value
                // position (failures surface as an outcome) and stays silent.
                let position = if discard {
                    Position::Statement
                } else {
                    Position::Value
                };
                let v = self.eval_expr(expr, position)?;
                if discard {
                    self.sink_value(&v);
                }
                last = Flow::Value(v);
                continue;
            }
            last = self.eval_stmt(stmt, false)?;
            if !matches!(last, Flow::Value(_)) {
                break;
            }
            // A discarded tail whose value is not a bare command (e.g. a nested
            // `if`/block that produced a command outcome) still routes to the
            // sink so loop-body side effects are not swallowed.
            if is_tail
                && sink_tail
                && let Flow::Value(v) = &last
            {
                self.sink_value(v);
            }
        }
        self.exec.env = old;
        Ok(last)
    }

    pub(crate) fn block_value(&mut self, b: &Block) -> VResult<Value> {
        match self.eval_block(b, false)? {
            Flow::Value(v) | Flow::Return(v) => Ok(v),
            Flow::Break | Flow::Continue => {
                Err(ErrorVal::new("custom", "loop control outside loop"))
            }
        }
    }
}
