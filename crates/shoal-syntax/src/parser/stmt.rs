//! Program/statement control flow: the top-level `statement` dispatcher (TDD
//! §3.1), the reserved-word statement forms (`let`/`export`/`fn`/`alias`/
//! `use`/`for`/`while`), block parsing, and the statement terminator check.

use super::*;

impl<'s> Parser<'s> {
    pub(crate) fn statement(&mut self) -> ParseResult<Stmt> {
        let start = self.lx.skip_trivia(self.pos);

        // REPL leading-`.` line: a postfix chain on `it` (§3.4). Excludes the
        // `./`…`../` path forms, which are command heads.
        if self.repl && self.byte(start) == b'.' && !self.is_path_head(start) {
            let seed = Expr::Var {
                name: "it".into(),
                span: Span::new(start, start),
            };
            let e = self.postfix(seed)?;
            let e = self.expr_tail(e, 0)?;
            let span = e.span();
            return Ok(Stmt::Expr { expr: e, span });
        }

        // Path-head command (`./deploy.sh`, `/bin/ls`, `~/x`). Detected from
        // raw bytes so EXPR-starters never reach a fatal CMD probe (D1).
        if self.is_path_head(start) {
            return self.command_stmt();
        }

        let (t, s) = self.peek(Mode::Expr)?;

        // §3.1 rule 1 — reserved-word constructs.
        if let Tok::Ident(k) = &t {
            match k.as_str() {
                "let" | "var" => return self.let_stmt(false),
                "export" => return self.export_stmt(),
                "fn" => return self.fn_stmt(false),
                "alias" => return self.alias_stmt(),
                "use" => return self.use_stmt(),
                "return" => {
                    self.bump(Mode::Expr)?;
                    let v = if self.at_end_stmt()? {
                        None
                    } else {
                        Some(self.expr(0)?)
                    };
                    return Ok(Stmt::Return {
                        value: v,
                        span: Span::new(start, self.pos),
                    });
                }
                "break" => {
                    self.bump(Mode::Expr)?;
                    return Ok(Stmt::Break { span: s });
                }
                "continue" => {
                    self.bump(Mode::Expr)?;
                    return Ok(Stmt::Continue { span: s });
                }
                "for" => return self.for_stmt(),
                "while" => return self.while_stmt(),
                _ => {}
            }
        }

        if let Tok::Ident(name) = t.clone() {
            // Keyword-headed expressions (`if`, `match`, literals, …) → EXPR.
            if matches!(
                name.as_str(),
                "true" | "false" | "null" | "if" | "match" | "try" | "with" | "spawn"
            ) {
                let expr = self.expr(0)?;
                let span = expr.span();
                return Ok(Stmt::Expr { expr, span });
            }

            // Interpreter block (IO.md §2.3): an interpreter-class head
            // immediately followed by `{`/`'''` is a `LangBlock` expression, not
            // a command. A head in the set *without* a following block falls
            // through to normal command dispatch (`python script.py` still runs
            // as a command).
            if INTERPRETERS.contains(&name.as_str()) && self.interp_block_follows(s) {
                let expr = self.expr(0)?;
                let span = expr.span();
                return Ok(Stmt::Expr { expr, span });
            }

            // `NAME=value cmd` is an environment prefix, while a standalone
            // `NAME=value` remains ordinary assignment. The CMD peek is made
            // non-fatal so a head that errors in CMD mode never propagates (D1).
            if let Ok((Tok::EnvAssign(_, _), env_span)) = self.peek(Mode::Cmd) {
                if !matches!(
                    self.lx
                        .token(env_span.end as usize, Mode::Cmd)
                        .map(|x| x.0)
                        .unwrap_or(Tok::Eof),
                    Tok::Newline | Tok::Semi | Tok::Eof
                ) {
                    return self.command_stmt();
                }
            }

            // `env.NAME = v` — session environment write (TDD §4.6): the
            // assignment lvalue additionally accepts a single-hop `Field`
            // target rooted at `env`, checked with the same tolerant,
            // restore-on-mismatch lookahead as the bare-identifier case below
            // so a non-`=` `env.NAME` (a plain read) falls through unchanged
            // to the ident-adjacency / command dispatch that follows.
            if name == "env" {
                let save = self.pos;
                self.bump(Mode::Expr)?; // `env`
                if let Ok((Tok::Dot, _)) = self.peek(Mode::Expr) {
                    self.bump(Mode::Expr)?; // `.`
                    if let Ok((Tok::Ident(field), field_span)) = self.peek(Mode::Expr) {
                        self.bump(Mode::Expr)?; // NAME
                        let next = self.peek(Mode::Expr).map(|x| x.0).unwrap_or(Tok::Eof);
                        if matches!(
                            next,
                            Tok::Eq | Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq
                        ) {
                            let target = Expr::Field {
                                recv: Box::new(Expr::Var {
                                    name: "env".into(),
                                    span: s,
                                }),
                                name: field,
                                optional: false,
                                span: Span::new(s.start as usize, field_span.end as usize),
                            };
                            let (op, _) = self.bump(Mode::Expr)?;
                            let value = self.expr(0)?;
                            return Ok(Stmt::Assign {
                                target,
                                op: assign_op(op),
                                value,
                                span: Span::new(start, self.pos),
                            });
                        }
                    }
                }
                self.pos = save;
            }

            // Assignment lookahead — tolerant of CMD-only tokens after the head
            // (e.g. a lone `&`, eval-audit #7): a peek error means "not `=`".
            let save = self.pos;
            self.bump(Mode::Expr)?;
            let next = self.peek(Mode::Expr).map(|x| x.0).unwrap_or(Tok::Eof);
            self.pos = save;
            if matches!(
                next,
                Tok::Eq | Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq
            ) {
                let target = self.primary(true)?;
                let (op, _) = self.bump(Mode::Expr)?;
                let value = self.expr(0)?;
                return Ok(Stmt::Assign {
                    target,
                    op: assign_op(op),
                    value,
                    span: Span::new(start, self.pos),
                });
            }

            // Ident-adjacency refinement (D3): `ls.where(…)` / `run("x", …)` —
            // an abutting `.`/`?.`/`(`/`[` forces an EXPR statement (the bare
            // command head desugars to `Var(name)` as the receiver; the
            // evaluator resolves an unbound `Var` to a zero-arg command).
            if self.adjacent_postfix_after_ident(s)? {
                let e = self.expr(0)?;
                let span = e.span();
                return Ok(Stmt::Expr { expr: e, span });
            }

            // Two-scope dispatch (D2): value-bound → EXPR; cmd-bound or unbound
            // → COMMAND.
            if self.bound(&name) {
                let e = self.expr(0)?;
                let span = e.span();
                return Ok(Stmt::Expr { expr: e, span });
            }
            return self.command_stmt();
        }

        // `^head …` forces command interpretation past shadowing.
        if matches!(t, Tok::Caret) {
            return self.command_stmt();
        }

        // §3.1 rule 2 — a non-identifier head is always an EXPR statement.
        let e = self.expr(0)?;
        let sp = e.span();
        Ok(Stmt::Expr { expr: e, span: sp })
    }
    /// After a statement, require a terminator (newline/`;`) unless the next
    /// token closes the enclosing scope (EBNF `{ statement TERM }`, D7). A
    /// stray bare word after a value expression gets the curated `^x` hint.
    pub(crate) fn require_term(&mut self, prev: &Stmt) -> ParseResult<()> {
        // The peek is tolerant of a CMD-only next head that lex-errors in EXPR
        // mode — that too is a missing terminator, not a fatal lex error.
        match self.peek(Mode::Expr) {
            Ok((Tok::Newline | Tok::Semi, _)) => {
                self.term()?;
                Ok(())
            }
            Ok((Tok::Eof | Tok::RBrace, _)) => Ok(()),
            other => {
                let s = match other {
                    Ok((_, s)) => s,
                    Err(e) => e.span,
                };
                let mut err = ParseError::new("expected newline or `;` between statements", s);
                if let Stmt::Expr {
                    expr: Expr::Var { name, .. },
                    ..
                } = prev
                {
                    err = err.hint(format!("did you mean the command? force it with `^{name}`"));
                }
                Err(err)
            }
        }
    }
    pub(crate) fn ty(&mut self) -> ParseResult<Type> {
        let (name, s) = self.ident()?;
        let mut args = vec![];
        if self.eat(Mode::Expr, &Tok::Lt)?.is_some() {
            loop {
                args.push(self.ty()?);
                if self.eat(Mode::Expr, &Tok::Comma)?.is_none() {
                    break;
                }
            }
            self.expect(Mode::Expr, Tok::Gt, "`>`")?;
        }
        let optional = self.eat(Mode::Expr, &Tok::Question)?.is_some();
        Ok(Type {
            name,
            args,
            optional,
            span: Span::new(s.start as usize, self.pos),
        })
    }
    pub(crate) fn let_stmt(&mut self, exported: bool) -> ParseResult<Stmt> {
        let start = self.bump(Mode::Expr)?.1.start as usize;
        let mutable = match &self.lx.src[start..self.pos] {
            x if x.trim() == "var" => true,
            _ => false,
        };
        let pat = self.pattern_bind()?;
        let ty = if self.eat(Mode::Expr, &Tok::Colon)?.is_some() {
            Some(self.ty()?)
        } else {
            None
        };
        self.expect(Mode::Expr, Tok::Eq, "`=`")?;
        let init = self.expr(0)?;
        if let Pattern::Bind { name, .. } = &pat {
            self.bind(name.clone())
        }
        Ok(Stmt::Let {
            pattern: pat,
            ty,
            init,
            mutable,
            exported,
            span: Span::new(start, self.pos),
        })
    }
    pub(crate) fn export_stmt(&mut self) -> ParseResult<Stmt> {
        self.bump(Mode::Expr)?;
        match self.peek(Mode::Expr)?.0 {
            Tok::Ident(ref x) if x == "let" || x == "var" => self.let_stmt(true),
            Tok::Ident(ref x) if x == "fn" => self.fn_stmt(true),
            _ => Err(ParseError::new(
                "export must precede let, var, or fn",
                self.peek(Mode::Expr)?.1,
            )),
        }
    }
    pub(crate) fn fn_stmt(&mut self, exported: bool) -> ParseResult<Stmt> {
        let start = self.bump(Mode::Expr)?.1.start as usize;
        let (name, _) = self.ident()?;
        self.expect(Mode::Expr, Tok::LParen, "`(`")?;
        let mut params = vec![];
        let mut rest = None;
        if self.eat(Mode::Expr, &Tok::RParen)?.is_none() {
            loop {
                if self.eat(Mode::Expr, &Tok::Ellipsis)?.is_some() {
                    let (n, _) = self.ident()?;
                    let ty = if self.eat(Mode::Expr, &Tok::Colon)?.is_some() {
                        Some(self.ty()?)
                    } else {
                        None
                    };
                    rest = Some(RestParam { name: n, ty });
                    self.expect(Mode::Expr, Tok::RParen, "`)`")?;
                    break;
                }
                let (n, s) = self.ident()?;
                let ty = if self.eat(Mode::Expr, &Tok::Colon)?.is_some() {
                    Some(self.ty()?)
                } else {
                    None
                };
                let default = if self.eat(Mode::Expr, &Tok::Eq)?.is_some() {
                    Some(self.expr(0)?)
                } else {
                    None
                };
                params.push(Param {
                    name: n,
                    ty,
                    default,
                    span: Span::new(s.start as usize, self.pos),
                });
                if self.eat(Mode::Expr, &Tok::Comma)?.is_some() {
                    if self.eat(Mode::Expr, &Tok::RParen)?.is_some() {
                        break;
                    }
                } else {
                    self.expect(Mode::Expr, Tok::RParen, "`)`")?;
                    break;
                }
            }
        }
        let ret = if self.eat(Mode::Expr, &Tok::ThinArrow)?.is_some() {
            Some(self.ty()?)
        } else {
            None
        };
        self.scopes
            .push(params.iter().map(|p| p.name.clone()).collect());
        if let Some(r) = &rest {
            self.bind(r.name.clone())
        }
        let body = self.block()?;
        self.scopes.pop();
        self.bind_cmd(name.clone());
        Ok(Stmt::Fn {
            decl: FnDecl {
                name,
                params,
                rest,
                ret,
                body,
                doc: None,
                exported,
                span: Span::new(start, self.pos),
            },
        })
    }
    pub(crate) fn alias_stmt(&mut self) -> ParseResult<Stmt> {
        let start = self.bump(Mode::Expr)?.1.start as usize;
        let (name, _) = self.ident()?;
        self.expect(Mode::Expr, Tok::Eq, "`=`")?;
        let target = self.command()?;
        self.bind_cmd(name.clone());
        Ok(Stmt::Alias {
            name,
            target,
            span: Span::new(start, self.pos),
        })
    }
    pub(crate) fn use_stmt(&mut self) -> ParseResult<Stmt> {
        let start = self.bump(Mode::Expr)?.1.start as usize;
        let (t, _) = self.bump(Mode::Cmd)?;
        let path = match t {
            Tok::Word(x) | Tok::PathWord(x) => x,
            _ => {
                return Err(ParseError::new(
                    "expected module path",
                    Span::new(self.pos, self.pos),
                ));
            }
        };
        Ok(Stmt::Use {
            path,
            span: Span::new(start, self.pos),
        })
    }
    pub(crate) fn for_stmt(&mut self) -> ParseResult<Stmt> {
        let start = self.bump(Mode::Expr)?.1.start as usize;
        let pattern = self.pattern_bind()?;
        match self.bump(Mode::Expr)? {
            (Tok::Ident(x), _) if x == "in" => {}
            (_, s) => return Err(ParseError::new("expected `in`", s)),
        }
        let iter = self.expr(0)?;
        self.scopes.push(HashSet::new());
        if let Pattern::Bind { name, .. } = &pattern {
            self.bind(name.clone())
        }
        let body = self.block()?;
        self.scopes.pop();
        Ok(Stmt::For {
            pattern,
            iter,
            body,
            span: Span::new(start, self.pos),
        })
    }
    pub(crate) fn while_stmt(&mut self) -> ParseResult<Stmt> {
        let start = self.bump(Mode::Expr)?.1.start as usize;
        let cond = self.expr(0)?;
        let body = self.block()?;
        Ok(Stmt::While {
            cond,
            body,
            span: Span::new(start, self.pos),
        })
    }

    pub(crate) fn block(&mut self) -> ParseResult<Block> {
        let open = self.expect(Mode::Expr, Tok::LBrace, "`{`")?;
        self.scopes.push(HashSet::new());
        self.cmd_scopes.push(HashSet::new());
        self.term()?;
        let mut stmts = vec![];
        while !self.peek_is(|t| matches!(t, Tok::RBrace | Tok::Eof)) {
            let stmt = self.statement()?;
            self.require_term(&stmt)?;
            stmts.push(stmt);
        }
        self.expect(Mode::Expr, Tok::RBrace, "`}`")?;
        self.scopes.pop();
        self.cmd_scopes.pop();
        Ok(Block {
            stmts,
            span: Span::new(open.start as usize, self.pos),
        })
    }
}

pub(crate) fn assign_op(t: Tok) -> AssignOp {
    match t {
        Tok::PlusEq => AssignOp::Add,
        Tok::MinusEq => AssignOp::Sub,
        Tok::StarEq => AssignOp::Mul,
        Tok::SlashEq => AssignOp::Div,
        _ => AssignOp::Set,
    }
}
