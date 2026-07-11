//! `match` pattern parsing (`match_pattern`/`match_expr`) and the small
//! runtime-type-name / pattern-binder helpers they use.

use super::*;

impl<'s> Parser<'s> {
    pub(crate) fn match_pattern(&mut self) -> ParseResult<Pattern> {
        let (t, s) = self.bump(Mode::Expr)?;
        Ok(match t {
            Tok::Ident(x) if x == "_" => Pattern::Wildcard { span: s },
            Tok::Ident(x) if x == "true" || x == "false" => Pattern::Lit {
                expr: Box::new(Expr::Bool {
                    value: x == "true",
                    span: s,
                }),
                span: s,
            },
            // Type pattern `TYPE IDENT` (`int n`, `str s`, …). Disambiguation
            // rule (TDD §3.2 `pat = … | IDENT | type IDENT | …`): a lone
            // identifier is always a bind — even when it names a type — so a
            // type pattern requires the type name to be *followed by a binder
            // identifier* (before `=>`/`|`/`if`). `int n` → Type; `int`, `n`,
            // and `n if …` → Bind.
            Tok::Ident(name) if is_type_name(&name) => match self.peek(Mode::Expr) {
                Ok((Tok::Ident(b), bs)) if b != "if" => {
                    self.bump(Mode::Expr)?;
                    Pattern::Type {
                        ty: Type {
                            name,
                            args: vec![],
                            optional: false,
                            span: s,
                        },
                        name: Some(b),
                        span: Span::new(s.start as usize, bs.end as usize),
                    }
                }
                _ => Pattern::Bind { name, span: s },
            },
            Tok::Ident(name) => Pattern::Bind { name, span: s },
            // Record pattern `{ field, field: subpat, … }` (open matching:
            // extra scrutinee fields are ignored).
            Tok::LBrace => {
                let mut fields = vec![];
                self.skip_newlines()?;
                if self.eat(Mode::Expr, &Tok::RBrace)?.is_none() {
                    loop {
                        self.skip_newlines()?;
                        let (fname, _) = self.ident()?;
                        let pattern = if self.eat(Mode::Expr, &Tok::Colon)?.is_some() {
                            Some(self.match_pattern()?)
                        } else {
                            None
                        };
                        fields.push(FieldPat {
                            name: fname,
                            pattern,
                        });
                        self.skip_newlines()?;
                        if self.eat(Mode::Expr, &Tok::Comma)?.is_none() {
                            self.expect(Mode::Expr, Tok::RBrace, "`}`")?;
                            break;
                        }
                        self.skip_newlines()?;
                        if self.eat(Mode::Expr, &Tok::RBrace)?.is_some() {
                            break;
                        }
                    }
                }
                Pattern::Record {
                    fields,
                    span: Span::new(s.start as usize, self.pos),
                }
            }
            // List pattern `[a, b, …rest]` (fixed arity unless `...rest`).
            Tok::LBracket => {
                let mut items = vec![];
                let mut rest = None;
                self.skip_newlines()?;
                if self.eat(Mode::Expr, &Tok::RBracket)?.is_none() {
                    loop {
                        self.skip_newlines()?;
                        if self.eat(Mode::Expr, &Tok::Ellipsis)?.is_some() {
                            let (rn, _) = self.ident()?;
                            rest = Some(rn);
                            self.skip_newlines()?;
                            self.expect(Mode::Expr, Tok::RBracket, "`]`")?;
                            break;
                        }
                        items.push(self.match_pattern()?);
                        self.skip_newlines()?;
                        if self.eat(Mode::Expr, &Tok::Comma)?.is_none() {
                            self.expect(Mode::Expr, Tok::RBracket, "`]`")?;
                            break;
                        }
                        self.skip_newlines()?;
                        if self.eat(Mode::Expr, &Tok::RBracket)?.is_some() {
                            break;
                        }
                    }
                }
                Pattern::List {
                    items,
                    rest,
                    span: Span::new(s.start as usize, self.pos),
                }
            }
            // Integer literal, or the start of a range pattern `a..b` / `a..=b`
            // (TDD §3.2 grammar: `pat = literal | rangepat | …`).
            Tok::Int(value) => {
                let start_expr = Expr::Int { value, span: s };
                if matches!(self.peek(Mode::Expr)?.0, Tok::DotDot | Tok::DotDotEq) {
                    let (dot, _) = self.bump(Mode::Expr)?;
                    let inclusive = matches!(dot, Tok::DotDotEq);
                    let (et, es) = self.bump(Mode::Expr)?;
                    let end_expr = match et {
                        Tok::Int(v) => Expr::Int { value: v, span: es },
                        _ => {
                            return Err(ParseError::new(
                                "expected an integer after `..` in a range pattern",
                                es,
                            ));
                        }
                    };
                    Pattern::Range {
                        start: Box::new(start_expr),
                        end: Box::new(end_expr),
                        inclusive,
                        span: Span::new(s.start as usize, es.end as usize),
                    }
                } else {
                    Pattern::Lit {
                        expr: Box::new(start_expr),
                        span: s,
                    }
                }
            }
            Tok::Str(value) => Pattern::Lit {
                expr: Box::new(Expr::Str { value, span: s }),
                span: s,
            },
            _ => return Err(ParseError::new("expected match pattern", s)),
        })
    }
    pub(crate) fn match_expr(&mut self, start: usize) -> ParseResult<Expr> {
        let scrutinee = self.expr(0)?;
        self.expect(Mode::Expr, Tok::LBrace, "`{`")?;
        self.term()?;
        let mut arms = Vec::new();
        while !matches!(self.peek(Mode::Expr)?.0, Tok::RBrace | Tok::Eof) {
            let arm_start = self.peek(Mode::Expr)?.1.start as usize;
            // A stray leading `|` gets the curated alternation teaching (D13).
            if let (Tok::Pipe, ps) = self.peek(Mode::Expr)? {
                return Err(
                    ParseError::new("unexpected `|` at the start of a match arm", ps)
                        .hint("alternation is `a | b => …`; drop the leading `|`"),
                );
            }
            let mut patterns = vec![self.match_pattern()?];
            while self.eat(Mode::Expr, &Tok::Pipe)?.is_some() {
                patterns.push(self.match_pattern()?);
            }
            // Pattern binders are in scope for the guard and body, so an
            // in-scope name (e.g. `status` in `{status} if status >= 200 && …`)
            // parses as a Var rather than dispatching a command (§3.1).
            self.scopes.push(HashSet::new());
            for p in &patterns {
                collect_pattern_binders(p, &mut |n| self.bind(n));
            }
            let guard = if matches!(&self.peek(Mode::Expr)?.0, Tok::Ident(x) if x == "if") {
                self.bump(Mode::Expr)?;
                Some(self.guard_expr()?)
            } else {
                None
            };
            self.expect(Mode::Expr, Tok::FatArrow, "`=>`")?;
            let body = if matches!(self.peek(Mode::Expr)?.0, Tok::LBrace) {
                let b = self.block()?;
                Expr::Block {
                    span: b.span,
                    block: b,
                }
            } else {
                self.expr(0)?
            };
            self.scopes.pop();
            arms.push(MatchArm {
                patterns,
                guard,
                span: Span::new(arm_start, body.span().end as usize),
                body,
            });
            // Arms are TERM-separated (TDD §3.2), but accept a trailing `,` as
            // an alternative terminator so Rust-style comma-separated arms
            // (`match x { 1 => a, 2 => b }`) parse too — a small friendly
            // superset.
            self.eat(Mode::Expr, &Tok::Comma)?;
            self.term()?;
        }
        self.expect(Mode::Expr, Tok::RBrace, "`}`")?;
        Ok(Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            span: Span::new(start, self.pos),
        })
    }
}

/// Known runtime type names usable as the head of a `type IDENT` match
/// pattern (TDD §3.2). Mirrors `shoal_value::Value::type_name`.
fn is_type_name(name: &str) -> bool {
    matches!(
        name,
        "null"
            | "bool"
            | "int"
            | "float"
            | "str"
            | "path"
            | "glob"
            | "regex"
            | "size"
            | "duration"
            | "datetime"
            | "time"
            | "bytes"
            | "list"
            | "record"
            | "table"
            | "range"
            | "stream"
            | "error"
            | "outcome"
            | "task"
            | "closure"
            | "command"
            | "secret"
    )
}
/// Walk a pattern, invoking `f` with each name it binds (bind idents, the
/// binder of a `type IDENT`, record shorthand/sub-pattern binders, list
/// element binders, and a `...rest` tail).
fn collect_pattern_binders(p: &Pattern, f: &mut impl FnMut(String)) {
    match p {
        Pattern::Bind { name, .. } => f(name.clone()),
        Pattern::Type { name: Some(n), .. } => f(n.clone()),
        Pattern::Record { fields, .. } => {
            for field in fields {
                match &field.pattern {
                    Some(sub) => collect_pattern_binders(sub, f),
                    None => f(field.name.clone()),
                }
            }
        }
        Pattern::List { items, rest, .. } => {
            for item in items {
                collect_pattern_binders(item, f);
            }
            if let Some(r) = rest {
                f(r.clone());
            }
        }
        Pattern::Wildcard { .. }
        | Pattern::Lit { .. }
        | Pattern::Range { .. }
        | Pattern::Type { name: None, .. } => {}
    }
}
