use crate::lexer::{LexError, Lexer, Mode, RESERVED, Seg, Tok};
use shoal_ast::*;
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub msg: String,
    pub span: Span,
    pub hint: Option<String>,
}
impl ParseError {
    fn new(msg: impl Into<String>, span: Span) -> Self {
        Self {
            msg: msg.into(),
            span,
            hint: None,
        }
    }
    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}
impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        Self {
            msg: e.msg,
            span: e.span,
            hint: e.hint,
        }
    }
}
impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}
impl std::error::Error for ParseError {}
pub type ParseResult<T> = Result<T, ParseError>;

/// Parse context carrying the dispatch inputs the parser cannot infer from the
/// source alone (TDD §3.1): whether we are at a REPL prompt (so `it`/`out` are
/// legal and a leading `.` chains on `it`), plus the pre-seeded value-bindings
/// (`let`/`var`/params) and command-bindings (session `fn`s/aliases).
#[derive(Debug, Clone, Default)]
pub struct ParseCtx {
    pub repl: bool,
    pub value_bound: Vec<String>,
    pub cmd_bound: Vec<String>,
}

pub fn parse(src: &str) -> ParseResult<Program> {
    Parser::new(src).parse_program()
}

pub fn parse_with_scope(src: &str, bound: impl IntoIterator<Item = String>) -> ParseResult<Program> {
    let mut parser = Parser::new(src);
    for name in bound {
        parser.bind(name);
    }
    parser.parse_program()
}

/// The full-fidelity entry point (TDD §3.1): dispatch honours the two scope
/// categories and the REPL context. `parse`/`parse_with_scope` are compat shims
/// mapping onto this with `repl: false` and no command bindings.
pub fn parse_with_ctx(src: &str, ctx: ParseCtx) -> ParseResult<Program> {
    let mut parser = Parser::new(src);
    parser.repl = ctx.repl;
    for name in ctx.value_bound {
        parser.bind(name);
    }
    for name in ctx.cmd_bound {
        parser.bind_cmd(name);
    }
    parser.parse_program()
}

pub struct Parser<'s> {
    lx: Lexer<'s>,
    pos: usize,
    /// Value bindings — `let`/`var`/params. A name here dispatches EXPR.
    scopes: Vec<HashSet<String>>,
    /// Command bindings — user `fn`s and aliases. A name here (and not also a
    /// value binding) dispatches CMD.
    cmd_scopes: Vec<HashSet<String>>,
    /// REPL context: `it`/`out` are legal and a leading `.` chains on `it`.
    repl: bool,
}

impl<'s> Parser<'s> {
    pub fn new(src: &'s str) -> Self {
        let builtins = ["path", "glob", "regex"]
            .into_iter()
            .map(str::to_string)
            .collect();
        Self {
            lx: Lexer::new(src),
            pos: 0,
            scopes: vec![builtins],
            cmd_scopes: vec![HashSet::new()],
            repl: false,
        }
    }
    fn peek(&self, m: Mode) -> ParseResult<(Tok, Span)> {
        Ok(self.lx.token(self.pos, m)?)
    }
    fn bump(&mut self, m: Mode) -> ParseResult<(Tok, Span)> {
        let x = self.peek(m)?;
        self.pos = x.1.end as usize;
        Ok(x)
    }
    fn eat(&mut self, m: Mode, want: &Tok) -> ParseResult<Option<Span>> {
        let (t, s) = self.peek(m)?;
        if std::mem::discriminant(&t) == std::mem::discriminant(want) {
            self.pos = s.end as usize;
            Ok(Some(s))
        } else {
            Ok(None)
        }
    }
    fn expect(&mut self, m: Mode, want: Tok, text: &str) -> ParseResult<Span> {
        self.eat(m, &want)?.ok_or_else(|| {
            let (_, s) = self
                .peek(m)
                .unwrap_or((Tok::Eof, Span::new(self.pos, self.pos)));
            ParseError::new(format!("expected {text}"), s)
        })
    }
    fn term(&mut self) -> ParseResult<()> {
        // Non-fatal: a head that lex-errors in EXPR mode (e.g. a `~/…` path
        // command) is not a terminator, so stop and let `statement()` dispatch.
        while let Ok((Tok::Newline | Tok::Semi, _)) = self.peek(Mode::Expr) {
            self.bump(Mode::Expr)?;
        }
        Ok(())
    }
    /// Peek the next EXPR token and test it; a lex error counts as "no match"
    /// so a CMD-only next head never aborts a statement loop's guard.
    fn peek_is(&self, f: impl Fn(&Tok) -> bool) -> bool {
        matches!(self.peek(Mode::Expr), Ok((t, _)) if f(&t))
    }
    fn bound(&self, n: &str) -> bool {
        self.scopes.iter().rev().any(|s| s.contains(n))
    }
    fn bind(&mut self, n: String) {
        self.scopes.last_mut().unwrap().insert(n);
    }
    fn bind_cmd(&mut self, n: String) {
        self.cmd_scopes.last_mut().unwrap().insert(n);
    }
    fn byte(&self, i: usize) -> u8 {
        self.lx.src.as_bytes().get(i).copied().unwrap_or(0)
    }
    /// Does the raw text at `start` begin a path literal (`./ ../ ~ ~/ /…`)?
    /// Such a head dispatches CMD (TDD §2.2 / §3.1 rule 2 is for EXPR starters,
    /// path words are command heads).
    fn is_path_head(&self, start: usize) -> bool {
        match self.byte(start) {
            b'/' => true,
            b'~' => {
                let n = self.byte(start + 1);
                n == b'/' || matches!(n, 0 | b' ' | b'\t' | b'\r' | b'\n' | b';')
            }
            b'.' => {
                let n = self.byte(start + 1);
                n == b'/' || (n == b'.' && self.byte(start + 2) == b'/')
            }
            _ => false,
        }
    }
    /// True when the token immediately after an identifier abuts it (no
    /// whitespace) and is a postfix opener `.`/`?.`/`(`/`[` — the §3.1
    /// ident-adjacency refinement forcing an EXPR statement.
    fn adjacent_postfix_after_ident(&self, ident_span: Span) -> ParseResult<bool> {
        Ok(match self.lx.token(ident_span.end as usize, Mode::Expr) {
            Ok((t, s)) => {
                s.start == ident_span.end
                    && matches!(
                        t,
                        Tok::Dot | Tok::QuestionDot | Tok::LParen | Tok::LBracket
                    )
            }
            Err(_) => false,
        })
    }
    /// Would the token stream at the current position dispatch as a COMMAND
    /// (used for `&&`/`||` operands and parenthesised command substitution)?
    fn at_command_head(&self) -> ParseResult<bool> {
        let start = self.lx.skip_trivia(self.pos);
        if self.byte(start) == b'^' {
            return Ok(true);
        }
        if self.is_path_head(start) {
            return Ok(true);
        }
        Ok(match self.peek(Mode::Expr) {
            Ok((Tok::Ident(name), s)) => {
                if RESERVED.contains(&name.as_str())
                    || matches!(name.as_str(), "with" | "spawn" | "sh")
                {
                    false
                } else if self.adjacent_postfix_after_ident(s)? {
                    false
                } else {
                    // value-bound → EXPR (Var); cmd-bound or unbound → command.
                    !self.bound(&name)
                }
            }
            _ => false,
        })
    }
    /// Consume a run of `Newline` tokens (delimiter-interior continuation, §2.1).
    fn skip_newlines(&mut self) -> ParseResult<()> {
        while matches!(self.peek(Mode::Expr)?.0, Tok::Newline) {
            self.bump(Mode::Expr)?;
        }
        Ok(())
    }
    /// Look past a run of newlines; if the next significant token satisfies
    /// `pred`, advance to just before it and return true (leading-`.`/`catch`/
    /// `else` cross-newline continuation, §2.1). Otherwise leave `pos` intact.
    fn continue_if<F: Fn(&Tok) -> bool>(&mut self, pred: F) -> ParseResult<bool> {
        let mut p = self.pos;
        loop {
            let (t, s) = match self.lx.token(p, Mode::Expr) {
                Ok(x) => x,
                Err(_) => return Ok(false),
            };
            match t {
                Tok::Newline => p = s.end as usize,
                _ if pred(&t) => {
                    self.pos = p;
                    return Ok(true);
                }
                _ => return Ok(false),
            }
        }
    }
    /// Parse a command operand (`Expr::Cmd`) when the head dispatches CMD,
    /// otherwise a normal expression. Used for `&&`/`||` operands and inside
    /// `(` … `)` group / command-substitution positions.
    fn expr_or_command(&mut self, min: u8) -> ParseResult<Expr> {
        if self.at_command_head()? {
            let call = self.command()?;
            let span = call.span;
            let e = Expr::Cmd {
                call: Box::new(call),
                span,
            };
            self.expr_tail(e, min)
        } else {
            self.expr(min)
        }
    }
    /// Dispatch and parse a COMMAND statement, then absorb any trailing
    /// `&&`/`||` command/expr operands (eval-audit #6).
    fn command_stmt(&mut self) -> ParseResult<Stmt> {
        let call = self.command()?;
        let cspan = call.span;
        let e = Expr::Cmd {
            call: Box::new(call),
            span: cspan,
        };
        let e = self.expr_tail(e, 0)?;
        let span = e.span();
        Ok(Stmt::Expr { expr: e, span })
    }

    pub fn parse_program(mut self) -> ParseResult<Program> {
        let mut stmts = vec![];
        self.term()?;
        while !self.peek_is(|t| matches!(t, Tok::Eof)) {
            let stmt = self.statement()?;
            self.require_term(&stmt)?;
            stmts.push(stmt);
        }
        Ok(Program { stmts })
    }

    fn statement(&mut self) -> ParseResult<Stmt> {
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
                "true" | "false" | "null" | "if" | "match" | "try" | "with" | "spawn" | "sh"
            ) {
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
                let target = self.primary()?;
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
    fn require_term(&mut self, prev: &Stmt) -> ParseResult<()> {
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
                    err = err.hint(format!(
                        "did you mean the command? force it with `^{name}`"
                    ));
                }
                Err(err)
            }
        }
    }
    fn at_end_stmt(&self) -> ParseResult<bool> {
        Ok(matches!(
            self.peek(Mode::Expr)?.0,
            Tok::Newline | Tok::Semi | Tok::Eof | Tok::RBrace
        ))
    }
    fn ident(&mut self) -> ParseResult<(String, Span)> {
        match self.bump(Mode::Expr)? {
            (Tok::Ident(n), s) => Ok((n, s)),
            (_, s) => Err(ParseError::new("expected identifier", s)),
        }
    }
    fn pattern_bind(&mut self) -> ParseResult<Pattern> {
        let (n, s) = self.ident()?;
        Ok(if n == "_" {
            Pattern::Wildcard { span: s }
        } else {
            Pattern::Bind { name: n, span: s }
        })
    }
    fn let_stmt(&mut self, exported: bool) -> ParseResult<Stmt> {
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
    fn export_stmt(&mut self) -> ParseResult<Stmt> {
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
    fn ty(&mut self) -> ParseResult<Type> {
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
    fn fn_stmt(&mut self, exported: bool) -> ParseResult<Stmt> {
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
    fn alias_stmt(&mut self) -> ParseResult<Stmt> {
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
    fn use_stmt(&mut self) -> ParseResult<Stmt> {
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
    fn for_stmt(&mut self) -> ParseResult<Stmt> {
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
    fn while_stmt(&mut self) -> ParseResult<Stmt> {
        let start = self.bump(Mode::Expr)?.1.start as usize;
        let cond = self.expr(0)?;
        let body = self.block()?;
        Ok(Stmt::While {
            cond,
            body,
            span: Span::new(start, self.pos),
        })
    }

    fn block(&mut self) -> ParseResult<Block> {
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

    fn command(&mut self) -> ParseResult<CmdCall> {
        let start = self.lx.skip_trivia(self.pos);
        let mut env_prefix = vec![];
        loop {
            match self.peek(Mode::Cmd)? {
                (Tok::EnvAssign(name, val), s) => {
                    self.bump(Mode::Cmd)?;
                    let value = CmdArg::Word { text: val, span: s };
                    env_prefix.push(EnvPrefix {
                        name,
                        value,
                        span: s,
                    })
                }
                _ => break,
            }
        }
        let forced = self.eat(Mode::Cmd, &Tok::Caret)?.is_some();
        // A path literal (`./x.sh`, `/bin/ls`, `~/x`) is a valid command head.
        let (head, _) = match self.bump(Mode::Cmd)? {
            (Tok::Word(x), s) | (Tok::PathWord(x), s) => (x, s),
            (x, s) => {
                return Err(ParseError::new(
                    format!("expected command head, found {x:?}"),
                    s,
                ));
            }
        };
        let mut args = vec![];
        let mut redirects = vec![];
        let mut background = false;
        let mut trailing = None;
        loop {
            let (t, s) = self.peek(Mode::Cmd)?;
            match t{Tok::Newline|Tok::Semi|Tok::Eof|Tok::RBrace|Tok::RParen|Tok::AndAnd|Tok::OrOr=>break,Tok::Pipe=>return Err(ParseError::new("shoal has no pipe operator",s).hint("data composes with `.`; raw byte plumbing is `.feed(cmd)`; verbatim POSIX lives in `sh { … }`")),Tok::Amp=>{self.bump(Mode::Cmd)?;background=true;break},Tok::LBrace=>{trailing=Some(self.block()?);break},Tok::RedirOut|Tok::RedirAppend|Tok::RedirIn=>{let kind=match self.bump(Mode::Cmd)?.0{Tok::RedirOut=>RedirectKind::Out,Tok::RedirAppend=>RedirectKind::Append,_=>RedirectKind::In};let target=self.cmd_arg()?;redirects.push(Redirect{kind,span:Span::new(s.start as usize,target.span().end as usize),target})},_=>args.push(self.cmd_arg()?)}
        }
        Ok(CmdCall {
            head,
            forced,
            args,
            redirects,
            env_prefix,
            background,
            trailing,
            span: Span::new(start, self.pos),
        })
    }
    fn cmd_arg(&mut self) -> ParseResult<CmdArg> {
        let (t, s) = self.bump(Mode::Cmd)?;
        Ok(match t {
            Tok::Word(text) => CmdArg::Word { text, span: s },
            Tok::PathWord(text) => CmdArg::Path { text, span: s },
            Tok::GlobWord(pattern) => CmdArg::Glob { pattern, span: s },
            Tok::Str(x) => CmdArg::Str {
                expr: Expr::Str { value: x, span: s },
                span: s,
            },
            Tok::StrInterp(x) => CmdArg::Str {
                expr: self.interp(x, s)?,
                span: s,
            },
            Tok::LParen => {
                let e = self.expr_or_command(0)?;
                self.expect(Mode::Expr, Tok::RParen, "`)`")?;
                CmdArg::Expr {
                    expr: e,
                    span: Span::new(s.start as usize, self.pos),
                }
            }
            Tok::FlagLong(name) => CmdArg::FlagLong {
                name,
                value: None,
                span: s,
            },
            Tok::FlagLongEq(name, v) => CmdArg::FlagLong {
                name,
                value: Some(Box::new(CmdArg::Word { text: v, span: s })),
                span: s,
            },
            Tok::FlagLongPendingValue(name) => {
                let v = self.cmd_arg()?;
                CmdArg::FlagLong {
                    name,
                    value: Some(Box::new(v)),
                    span: Span::new(s.start as usize, self.pos),
                }
            }
            Tok::FlagShort(chars) => CmdArg::FlagShort { chars, span: s },
            Tok::DashDash => CmdArg::DashDash { span: s },
            Tok::Dash => CmdArg::Dash { span: s },
            _ => return Err(ParseError::new("expected command argument", s)),
        })
    }

    fn expr(&mut self, min: u8) -> ParseResult<Expr> {
        let lhs = self.unary()?;
        self.expr_tail(lhs, min)
    }
    fn expr_tail(&mut self, mut lhs: Expr, min: u8) -> ParseResult<Expr> {
        loop {
            let (mut t, _) = self.peek(Mode::Expr)?;
            // Cross-newline continuation (§2.1): a trailing binary operator (the
            // newline is already inside an open subexpression) never reaches
            // here, but a `catch` on the *next* line must still attach.
            if min == 0 && matches!(t, Tok::Newline) {
                if self.continue_if(|t| matches!(t, Tok::Ident(x) if x == "catch"))? {
                    t = self.peek(Mode::Expr)?.0;
                } else {
                    break;
                }
            }
            if min == 0 && matches!(&t, Tok::Ident(x) if x == "catch") {
                self.bump(Mode::Expr)?;
                let binder = if let (Tok::Ident(name), _) = self.peek(Mode::Expr)? {
                    self.bump(Mode::Expr)?;
                    Some(name)
                } else {
                    None
                };
                let handler = if matches!(self.peek(Mode::Expr)?.0, Tok::LBrace) {
                    let block = self.block()?;
                    Expr::Block {
                        span: block.span,
                        block,
                    }
                } else {
                    self.expr(0)?
                };
                let span = Span::new(lhs.span().start as usize, handler.span().end as usize);
                lhs = Expr::Catch {
                    expr: Box::new(lhs),
                    binder,
                    handler: Box::new(handler),
                    span,
                };
                continue;
            }
            let Some((bp, op)) = binop(&t) else { break };
            if bp < min {
                break;
            }
            if is_comparison_token(&t)
                && matches!(
                    lhs,
                    Expr::Binary {
                        op: BinOp::Eq
                            | BinOp::Ne
                            | BinOp::Lt
                            | BinOp::Le
                            | BinOp::Gt
                            | BinOp::Ge
                            | BinOp::In,
                        ..
                    }
                )
            {
                return Err(ParseError::new(
                    "comparison operators do not chain",
                    self.peek(Mode::Expr)?.1,
                )
                .hint("combine comparisons explicitly with `&&`"));
            }
            self.bump(Mode::Expr)?;
            // A trailing binary operator continues the statement across newlines
            // (§2.1); `&&`/`||` operands may be commands (eval-audit #6).
            self.skip_newlines()?;
            let rhs = if matches!(op, Some(BinOp::And) | Some(BinOp::Or)) {
                self.expr_or_command(bp + 1)?
            } else {
                self.expr(bp + 1)?
            };
            let span = Span::new(lhs.span().start as usize, rhs.span().end as usize);
            lhs = if matches!(t, Tok::DotDot | Tok::DotDotEq) {
                Expr::Range {
                    start: Box::new(lhs),
                    end: Box::new(rhs),
                    inclusive: matches!(t, Tok::DotDotEq),
                    span,
                }
            } else {
                Expr::Binary {
                    op: op.unwrap(),
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    span,
                }
            };
        }
        Ok(lhs)
    }
    fn unary(&mut self) -> ParseResult<Expr> {
        let (t, s) = self.peek(Mode::Expr)?;
        if matches!(t, Tok::Bang | Tok::Minus) {
            self.bump(Mode::Expr)?;
            let e = self.unary()?;
            let end = e.span().end;
            return Ok(Expr::Unary {
                op: if matches!(t, Tok::Bang) {
                    UnOp::Not
                } else {
                    UnOp::Neg
                },
                expr: Box::new(e),
                span: Span::new(s.start as usize, end as usize),
            });
        }
        let p = self.primary()?;
        self.postfix(p)
    }
    fn primary(&mut self) -> ParseResult<Expr> {
        let (t, s) = self.bump(Mode::Expr)?;
        Ok(match t{Tok::Int(value)=>Expr::Int{value,span:s},Tok::Float(value)=>Expr::Float{value,span:s},Tok::Size(bytes)=>Expr::Size{bytes,span:s},Tok::Duration(ns)=>Expr::Duration{ns,span:s},Tok::Time{hour,min,sec}=>Expr::Time{hour,min,sec,span:s},Tok::Str(value)=>Expr::Str{value,span:s},Tok::StrInterp(parts)=>self.interp(parts,s)?,Tok::Regex(src)=>Expr::Regex{src,span:s},Tok::DateTime(iso)=>Expr::DateTime{iso,span:s},Tok::Ident(x)if x=="true"||x=="false"=>Expr::Bool{value:x=="true",span:s},Tok::Ident(x)if x=="null"=>Expr::Null{span:s},Tok::Ident(x)if x=="if"=>return self.if_expr(s.start as usize),Tok::Ident(x)if x=="try"=>return self.try_expr(s.start as usize),Tok::Ident(x)if x=="match"=>return self.match_expr(s.start as usize),Tok::Ident(x)if x=="with"=>return self.with_expr(s.start as usize),Tok::Ident(x)if x=="spawn"=>{let body=self.block()?;Expr::Spawn{body,span:Span::new(s.start as usize,self.pos)}},Tok::Ident(x)if x=="sh"=>{if self.byte(self.pos)==b'\''{let(rt,rs)=self.bump(Mode::Expr)?;match rt{Tok::Str(src)=>Expr::ShRaw{src,span:Span::new(s.start as usize,rs.end as usize)},_=>return Err(ParseError::new("expected sh payload after `sh'`",rs))}}else{let open=self.expect(Mode::Expr,Tok::LBrace,"`{` or `'''…'''`")?;let(src,end)=self.lx.raw_brace_block(open.start as usize)?;self.pos=end;Expr::ShRaw{src,span:Span::new(s.start as usize,end as usize)}}},Tok::Ident(name)=>{if matches!(self.peek(Mode::Expr)?.0,Tok::FatArrow){self.bump(Mode::Expr)?;let body=self.expr(0)?;let end=body.span().end;Expr::Lambda{params:vec![Param{name,ty:None,default:None,span:s}],body:Box::new(body),span:Span::new(s.start as usize,end as usize)}}else{if !self.repl&&matches!(name.as_str(),"it"|"out"){return Err(ParseError::new(format!("`{name}` is REPL-only"),s).hint("bind a variable to reuse a previous result"))}Expr::Var{name,span:s}}},Tok::LParen=>return self.paren_or_lambda(s.start as usize),Tok::LBracket=>{let mut items=vec![];self.skip_newlines()?;if self.eat(Mode::Expr,&Tok::RBracket)?.is_none(){loop{items.push(self.expr(0)?);self.skip_newlines()?;if self.eat(Mode::Expr,&Tok::Comma)?.is_none(){self.expect(Mode::Expr,Tok::RBracket,"`]`")?;break}self.skip_newlines()?;if self.eat(Mode::Expr,&Tok::RBracket)?.is_some(){break}}}Expr::List{items,span:Span::new(s.start as usize,self.pos)}},Tok::LBrace=>return self.record_or_block(s.start as usize),Tok::Pipe=>return Err(ParseError::new("shoal has no pipe operator",s).hint("data composes with `.`; raw byte plumbing is `.feed(cmd)`; verbatim POSIX lives in `sh { … }`")),_=>return Err(ParseError::new(format!("expected expression, found {t:?}"),s))})
    }
    fn postfix(&mut self, mut e: Expr) -> ParseResult<Expr> {
        loop {
            let (t, _) = self.peek(Mode::Expr)?;
            match t {
                // Leading-`.` on the next line continues this postfix chain
                // (§2.1). A `[`/`(` on the next line does *not* continue.
                Tok::Newline => {
                    if self.continue_if(|t| matches!(t, Tok::Dot | Tok::QuestionDot))? {
                        continue;
                    }
                    break;
                }
                Tok::Dot | Tok::QuestionDot => {
                    let optional = matches!(self.bump(Mode::Expr)?.0, Tok::QuestionDot);
                    let (name, _) = self.ident()?;
                    if self.eat(Mode::Expr, &Tok::LParen)?.is_some() {
                        let mut args = self.args_after_open()?;
                        // Trailing block after a method call (§3.4 `f(a){…}`).
                        if matches!(self.peek(Mode::Expr)?.0, Tok::LBrace) {
                            args.pos.push(self.trailing_block_lambda()?);
                        }
                        let span = Span::new(e.span().start as usize, self.pos);
                        e = Expr::MethodCall {
                            recv: Box::new(e),
                            name,
                            args,
                            optional,
                            span,
                        }
                    } else if !optional && matches!(self.peek(Mode::Expr)?.0, Tok::LBrace) {
                        // `xs.each { … }` — method call with only a thunk arg.
                        let mut args = Args::empty();
                        args.pos.push(self.trailing_block_lambda()?);
                        let span = Span::new(e.span().start as usize, self.pos);
                        e = Expr::MethodCall {
                            recv: Box::new(e),
                            name,
                            args,
                            optional,
                            span,
                        }
                    } else {
                        let span = Span::new(e.span().start as usize, self.pos);
                        e = Expr::Field {
                            recv: Box::new(e),
                            name,
                            optional,
                            span,
                        }
                    }
                }
                Tok::LBracket => {
                    self.bump(Mode::Expr)?;
                    let i = self.expr(0)?;
                    self.expect(Mode::Expr, Tok::RBracket, "`]`")?;
                    let span = Span::new(e.span().start as usize, self.pos);
                    e = Expr::Index {
                        recv: Box::new(e),
                        index: Box::new(i),
                        span,
                    }
                }
                Tok::LParen => {
                    self.bump(Mode::Expr)?;
                    let mut args = self.args_after_open()?;
                    // Trailing block after a call (§3.4 `f(a){…}`).
                    if matches!(self.peek(Mode::Expr)?.0, Tok::LBrace) {
                        args.pos.push(self.trailing_block_lambda()?);
                    }
                    let span = Span::new(e.span().start as usize, self.pos);
                    match e {
                        Expr::Var { name, .. } => e = Expr::FnCall { name, args, span },
                        _ => {
                            return Err(ParseError::new(
                                "only a named function can be called directly",
                                span,
                            ));
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(e)
    }
    /// Parse a trailing `{ … }` block as a zero-argument lambda thunk
    /// (`() => { … }`), per the §3.4 `f(a){…}` desugar.
    fn trailing_block_lambda(&mut self) -> ParseResult<Expr> {
        let block = self.block()?;
        let span = block.span;
        Ok(Expr::Lambda {
            params: vec![],
            body: Box::new(Expr::Block { block, span }),
            span,
        })
    }
    fn args_after_open(&mut self) -> ParseResult<Args> {
        let mut a = Args::empty();
        self.skip_newlines()?;
        if self.eat(Mode::Expr, &Tok::RParen)?.is_some() {
            return Ok(a);
        }
        loop {
            if let (Tok::Dot, dot) = self.peek(Mode::Expr)? {
                let seed = Expr::Var {
                    name: "__item".into(),
                    span: dot,
                };
                let chained = self.postfix(seed)?;
                let body = self.expr_tail(chained, 0)?;
                let end = body.span().end;
                a.pos.push(Expr::Lambda {
                    params: vec![Param {
                        name: "__item".into(),
                        ty: None,
                        default: None,
                        span: dot,
                    }],
                    body: Box::new(body),
                    span: Span::new(dot.start as usize, end as usize),
                });
            } else {
                let save = self.pos;
                if let (Tok::Ident(n), s) = self.peek(Mode::Expr)? {
                    self.bump(Mode::Expr)?;
                    if self.eat(Mode::Expr, &Tok::Colon)?.is_some() {
                        let v = self.expr(0)?;
                        a.named.push(NamedArg {
                            name: n,
                            value: v,
                            span: Span::new(s.start as usize, self.pos),
                        })
                    } else {
                        self.pos = save;
                        a.pos.push(self.expr(0)?);
                    }
                } else {
                    a.pos.push(self.expr(0)?)
                }
            }
            self.skip_newlines()?;
            if self.eat(Mode::Expr, &Tok::Comma)?.is_none() {
                self.expect(Mode::Expr, Tok::RParen, "`)`")?;
                break;
            }
            self.skip_newlines()?;
            if self.eat(Mode::Expr, &Tok::RParen)?.is_some() {
                break;
            }
        }
        Ok(a)
    }
    fn interp(&self, segs: Vec<Seg>, span: Span) -> ParseResult<Expr> {
        let mut parts = vec![];
        for x in segs {
            match x {
                Seg::Lit(text) => parts.push(StrPart::Lit { text }),
                Seg::Expr { start, end } => {
                    let mut parser = Parser::new(&self.lx.src[start as usize..end as usize]);
                    parser.scopes = self.scopes.clone();
                    parser.cmd_scopes = self.cmd_scopes.clone();
                    parser.repl = self.repl;
                    let mut e = parser.parse_program()?;
                    if e.stmts.len() != 1 {
                        return Err(ParseError::new(
                            "interpolation must contain one expression",
                            Span::new(start as usize, end as usize),
                        ));
                    }
                    match e.stmts.remove(0) {
                        Stmt::Expr { expr, .. } => parts.push(StrPart::Expr { expr }),
                        _ => {
                            return Err(ParseError::new(
                                "interpolation must be an expression",
                                Span::new(start as usize, end as usize),
                            ));
                        }
                    }
                }
            }
        }
        Ok(Expr::StrInterp { parts, span })
    }
    fn paren_or_lambda(&mut self, start: usize) -> ParseResult<Expr> {
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
    fn match_pattern(&mut self) -> ParseResult<Pattern> {
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
            Tok::Ident(name) => Pattern::Bind { name, span: s },
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
                            ))
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
    fn match_expr(&mut self, start: usize) -> ParseResult<Expr> {
        let scrutinee = self.expr(0)?;
        self.expect(Mode::Expr, Tok::LBrace, "`{`")?;
        self.term()?;
        let mut arms = Vec::new();
        while !matches!(self.peek(Mode::Expr)?.0, Tok::RBrace | Tok::Eof) {
            let arm_start = self.peek(Mode::Expr)?.1.start as usize;
            // A stray leading `|` gets the curated alternation teaching (D13).
            if let (Tok::Pipe, ps) = self.peek(Mode::Expr)? {
                return Err(ParseError::new("unexpected `|` at the start of a match arm", ps)
                    .hint("alternation is `a | b => …`; drop the leading `|`"));
            }
            let mut patterns = vec![self.match_pattern()?];
            while self.eat(Mode::Expr, &Tok::Pipe)?.is_some() {
                patterns.push(self.match_pattern()?);
            }
            let guard = if matches!(&self.peek(Mode::Expr)?.0, Tok::Ident(x) if x == "if") {
                self.bump(Mode::Expr)?;
                Some(self.expr(0)?)
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
            arms.push(MatchArm {
                patterns,
                guard,
                span: Span::new(arm_start, body.span().end as usize),
                body,
            });
            self.term()?;
        }
        self.expect(Mode::Expr, Tok::RBrace, "`}`")?;
        Ok(Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            span: Span::new(start, self.pos),
        })
    }
    fn with_expr(&mut self, start: usize) -> ParseResult<Expr> {
        let mut cwd = None;
        let mut env = None;
        loop {
            let (name, s) = self.ident()?;
            self.expect(Mode::Expr, Tok::Colon, "`:`")?;
            let value = self.expr(0)?;
            match name.as_str() {
                "cwd" => cwd = Some(Box::new(value)),
                "env" => env = Some(Box::new(value)),
                _ => return Err(ParseError::new("with accepts only cwd and env", s)),
            }
            if self.eat(Mode::Expr, &Tok::Comma)?.is_none() {
                break;
            }
        }
        let body = self.block()?;
        Ok(Expr::With {
            cwd,
            env,
            body,
            span: Span::new(start, self.pos),
        })
    }
    fn record_or_block(&mut self, start: usize) -> ParseResult<Expr> {
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
    fn if_expr(&mut self, start: usize) -> ParseResult<Expr> {
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
    fn try_expr(&mut self, start: usize) -> ParseResult<Expr> {
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
}
fn assign_op(t: Tok) -> AssignOp {
    match t {
        Tok::PlusEq => AssignOp::Add,
        Tok::MinusEq => AssignOp::Sub,
        Tok::StarEq => AssignOp::Mul,
        Tok::SlashEq => AssignOp::Div,
        _ => AssignOp::Set,
    }
}
fn binop(t: &Tok) -> Option<(u8, Option<BinOp>)> {
    Some(match t {
        Tok::QQ => (1, Some(BinOp::Coalesce)),
        Tok::OrOr => (2, Some(BinOp::Or)),
        Tok::AndAnd => (3, Some(BinOp::And)),
        Tok::EqEq => (4, Some(BinOp::Eq)),
        Tok::NotEq => (4, Some(BinOp::Ne)),
        Tok::Lt => (4, Some(BinOp::Lt)),
        Tok::Le => (4, Some(BinOp::Le)),
        Tok::Gt => (4, Some(BinOp::Gt)),
        Tok::Ge => (4, Some(BinOp::Ge)),
        Tok::Ident(x) if x == "in" => (4, Some(BinOp::In)),
        Tok::DotDot | Tok::DotDotEq => (5, None),
        Tok::Plus => (6, Some(BinOp::Add)),
        Tok::Minus => (6, Some(BinOp::Sub)),
        Tok::Star => (7, Some(BinOp::Mul)),
        Tok::Slash => (7, Some(BinOp::Div)),
        Tok::Percent => (7, Some(BinOp::Rem)),
        _ => return None,
    })
}

fn is_comparison_token(t: &Tok) -> bool {
    matches!(
        t,
        Tok::EqEq | Tok::NotEq | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge
    ) || matches!(t, Tok::Ident(x) if x == "in")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arithmetic_precedence() {
        let p = parse("let x = 2 + 3 * 4\nx - 1").unwrap();
        assert_eq!(p.stmts.len(), 2);
        match &p.stmts[0] {
            Stmt::Let {
                init:
                    Expr::Binary {
                        op: BinOp::Add,
                        rhs,
                        ..
                    },
                ..
            } => assert!(matches!(**rhs, Expr::Binary { op: BinOp::Mul, .. })),
            x => panic!("{x:?}"),
        }
    }
    #[test]
    fn command_shape() {
        let p = parse("FOO=x git push *.rs --force > out &").unwrap();
        match &p.stmts[0] {
            Stmt::Expr {
                expr: Expr::Cmd { call, .. },
                ..
            } => {
                assert_eq!(call.head, "git");
                assert_eq!(call.env_prefix.len(), 1);
                assert!(call.background);
                assert_eq!(call.redirects.len(), 1)
            }
            x => panic!("{x:?}"),
        }
    }
    #[test]
    fn declarations_and_fn() {
        let p = parse("fn add(a: int, b: int = 1) -> int { a + b }\nlet z = add(2)").unwrap();
        assert!(matches!(p.stmts[0], Stmt::Fn { .. }));
    }
    #[test]
    fn teaching_pipe_error() {
        let e = parse("ls | wc").unwrap_err();
        assert!(e.msg.contains("no pipe operator"));
    }
    #[test]
    fn records_lists_and_chain() {
        let p = parse("let xs = [{name: \"a\"}]\nxs.where(.name == \"a\")").unwrap();
        match &p.stmts[1] {
            Stmt::Expr {
                expr: Expr::MethodCall { args, .. },
                ..
            } => assert!(matches!(args.pos[0], Expr::Lambda { .. })),
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn logical_operators_and_precedence() {
        for src in ["false && missing", "true || missing", "null ?? 3"] {
            assert!(parse(src).is_ok(), "failed to parse {src}");
        }
        let p = parse("true || false && null ?? 3").unwrap();
        match &p.stmts[0] {
            Stmt::Expr {
                expr:
                    Expr::Binary {
                        op: BinOp::Coalesce,
                        lhs,
                        ..
                    },
                ..
            } => assert!(matches!(**lhs, Expr::Binary { op: BinOp::Or, .. })),
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn remaining_expression_forms() {
        let p = parse("match 1 { 0 | 1 => \"bit\"\n _ => \"other\" }").unwrap();
        assert!(matches!(
            p.stmts[0],
            Stmt::Expr {
                expr: Expr::Match { .. },
                ..
            }
        ));
        let p = parse("with cwd: \"/tmp\", env: {A: \"b\"} { 1 }").unwrap();
        assert!(matches!(
            p.stmts[0],
            Stmt::Expr {
                expr: Expr::With { .. },
                ..
            }
        ));
        let p = parse("let f = (a: int, b: int) => a + b\nf(1, 2) catch { 0 }").unwrap();
        assert!(matches!(
            p.stmts[0],
            Stmt::Let {
                init: Expr::Lambda { .. },
                ..
            }
        ));
        assert!(matches!(
            p.stmts[1],
            Stmt::Expr {
                expr: Expr::Catch { .. },
                ..
            }
        ));
    }
}
