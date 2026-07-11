//! The remaining brace/paren-headed expression forms: `(...)` group vs.
//! lambda disambiguation, `{ ... }` record-vs-block disambiguation, and the
//! `if`/`try`/`with` control-flow expressions.

use super::*;

impl<'s> Parser<'s> {
    pub(crate) fn with_expr(&mut self, start: usize) -> ParseResult<Expr> {
        let mut cwd = None;
        let mut env = None;
        let mut reef = None;
        loop {
            let (name, s) = self.ident()?;
            self.expect(Mode::Expr, Tok::Colon, "`:`")?;
            let value = self.expr(0)?;
            match name.as_str() {
                "cwd" => cwd = Some(Box::new(value)),
                "env" => env = Some(Box::new(value)),
                "reef" => reef = Some(Box::new(value)),
                _ => return Err(ParseError::new("with accepts only cwd, env, and reef", s)),
            }
            if self.eat(Mode::Expr, &Tok::Comma)?.is_none() {
                break;
            }
        }
        let body = self.block()?;
        Ok(Expr::With {
            cwd,
            env,
            reef,
            body,
            span: Span::new(start, self.pos),
        })
    }
    pub(crate) fn record_or_block(&mut self, start: usize) -> ParseResult<Expr> {
        if self.eat(Mode::Expr, &Tok::RBrace)?.is_some() {
            return Ok(Expr::Record {
                fields: vec![],
                span: Span::new(start, self.pos),
            });
        }
        let save = self.pos;
        if let (Tok::Ident(name) | Tok::Str(name), s) = self.bump(Mode::Expr)? {
            if self.eat(Mode::Expr, &Tok::Colon)?.is_some() {
                let mut fields = vec![RecordField {
                    name,
                    value: self.expr(0)?,
                    span: s,
                }];
                self.skip_newlines()?;
                while self.eat(Mode::Expr, &Tok::Comma)?.is_some() {
                    self.skip_newlines()?;
                    if self.eat(Mode::Expr, &Tok::RBrace)?.is_some() {
                        return Ok(Expr::Record {
                            fields,
                            span: Span::new(start, self.pos),
                        });
                    }
                    let (n, ns) = match self.bump(Mode::Expr)? {
                        (Tok::Ident(n), s) | (Tok::Str(n), s) => (n, s),
                        (_, s) => return Err(ParseError::new("expected record field", s)),
                    };
                    self.expect(Mode::Expr, Tok::Colon, "`:`")?;
                    fields.push(RecordField {
                        name: n,
                        value: self.expr(0)?,
                        span: ns,
                    });
                    self.skip_newlines()?;
                }
                self.expect(Mode::Expr, Tok::RBrace, "`}`")?;
                return Ok(Expr::Record {
                    fields,
                    span: Span::new(start, self.pos),
                });
            }
        }
        self.pos = save;
        self.scopes.push(HashSet::new());
        self.cmd_scopes.push(HashSet::new());
        let mut stmts = vec![];
        self.term()?;
        while !self.peek_is(|t| matches!(t, Tok::RBrace | Tok::Eof)) {
            let stmt = self.statement()?;
            self.require_term(&stmt)?;
            stmts.push(stmt);
        }
        self.expect(Mode::Expr, Tok::RBrace, "`}`")?;
        self.scopes.pop();
        self.cmd_scopes.pop();
        Ok(Expr::Block {
            block: Block {
                stmts,
                span: Span::new(start, self.pos),
            },
            span: Span::new(start, self.pos),
        })
    }
    pub(crate) fn if_expr(&mut self, start: usize) -> ParseResult<Expr> {
        let cond = self.expr(0)?;
        let then = self.block()?;
        // `else` may appear on the next line (§2.1 continuation).
        if matches!(self.peek(Mode::Expr)?.0, Tok::Newline) {
            self.continue_if(|t| matches!(t, Tok::Ident(x) if x == "else"))?;
        }
        let els = if let (Tok::Ident(x), _) = self.peek(Mode::Expr)? {
            if x == "else" {
                self.bump(Mode::Expr)?;
                if let (Tok::Ident(y), s) = self.peek(Mode::Expr)? {
                    if y == "if" {
                        self.bump(Mode::Expr)?;
                        Some(Box::new(self.if_expr(s.start as usize)?))
                    } else {
                        let b = self.block()?;
                        Some(Box::new(Expr::Block {
                            span: b.span,
                            block: b,
                        }))
                    }
                } else {
                    let b = self.block()?;
                    Some(Box::new(Expr::Block {
                        span: b.span,
                        block: b,
                    }))
                }
            } else {
                None
            }
        } else {
            None
        };
        Ok(Expr::If {
            cond: Box::new(cond),
            then,
            r#else: els,
            span: Span::new(start, self.pos),
        })
    }
    pub(crate) fn try_expr(&mut self, start: usize) -> ParseResult<Expr> {
        let body = self.block()?;
        match self.bump(Mode::Expr)? {
            (Tok::Ident(x), _) if x == "catch" => {}
            (_, s) => return Err(ParseError::new("expected catch", s)),
        }
        let pattern = if matches!(self.peek(Mode::Expr)?.0, Tok::LBrace) {
            None
        } else {
            Some(self.pattern_bind()?)
        };
        let handler = self.block()?;
        Ok(Expr::Try {
            body,
            pattern,
            handler,
            span: Span::new(start, self.pos),
        })
    }
    pub(crate) fn paren_or_lambda(&mut self, start: usize) -> ParseResult<Expr> {
        let save = self.pos;
        let mut params = Vec::new();
        let mut plausible = true;
        if self.eat(Mode::Expr, &Tok::RParen)?.is_none() {
            loop {
                let Ok((name, ps)) = self.ident() else {
                    plausible = false;
                    break;
                };
                let ty = if self.eat(Mode::Expr, &Tok::Colon)?.is_some() {
                    Some(self.ty()?)
                } else {
                    None
                };
                params.push(Param {
                    name,
                    ty,
                    default: None,
                    span: ps,
                });
                if self.eat(Mode::Expr, &Tok::Comma)?.is_some() {
                    continue;
                }
                if self.eat(Mode::Expr, &Tok::RParen)?.is_none() {
                    plausible = false;
                }
                break;
            }
        }
        if plausible && self.eat(Mode::Expr, &Tok::FatArrow)?.is_some() {
            let body = if matches!(self.peek(Mode::Expr)?.0, Tok::LBrace) {
                let block = self.block()?;
                Expr::Block {
                    span: block.span,
                    block,
                }
            } else {
                self.expr(0)?
            };
            let end = body.span().end;
            return Ok(Expr::Lambda {
                params,
                body: Box::new(body),
                span: Span::new(start, end as usize),
            });
        }
        self.pos = save;
        // Not a lambda: a parenthesised group. Apply the same two-mode dispatch
        // as `statement()` so `(echo hi)` runs the command (substitution, D4).
        self.skip_newlines()?;
        let expr = self.expr_or_command(0)?;
        self.skip_newlines()?;
        self.expect(Mode::Expr, Tok::RParen, "`)`")?;
        Ok(expr)
    }
}
